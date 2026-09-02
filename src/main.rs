mod apns;
mod ratelimit;
mod stats;

use std::{
    collections::VecDeque,
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Json,
    Router,
};
use dashmap::DashMap;
use futures::{stream, Stream, StreamExt};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

/// Max messages buffered per channel while no listener is connected.
const MAX_QUEUE: usize = 5;
/// Empty, idle channels are swept after this long.
const IDLE_TTL: Duration = Duration::from_secs(300);
/// Longest accepted channel id / apns token path segment.
const MAX_ID_LEN: usize = 256;

/// Queued messages exist only to bridge dropped-connection recovery: a
/// message older than this is never delivered. Overridable for testing via
/// QUEUE_TTL_SECS.
fn msg_ttl() -> Duration {
    static TTL: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *TTL.get_or_init(|| {
        Duration::from_secs(
            std::env::var("QUEUE_TTL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
        )
    })
}

struct Channel {
    tx: broadcast::Sender<String>,
    queue: Mutex<VecDeque<(Instant, String)>>,
    touched: Mutex<Instant>,
}

impl Channel {
    fn new() -> Self {
        Self {
            tx: broadcast::channel(64).0,
            queue: Mutex::new(VecDeque::with_capacity(MAX_QUEUE)),
            touched: Mutex::new(Instant::now()),
        }
    }

    fn touch(&self) {
        *self.touched.lock().unwrap() = Instant::now();
    }

    fn enqueue(&self, msg: String) {
        let mut queue = self.queue.lock().unwrap();
        queue.retain(|(queued_at, _)| queued_at.elapsed() < msg_ttl());
        if queue.len() >= MAX_QUEUE {
            queue.pop_front();
        }
        queue.push_back((Instant::now(), msg));
    }

    fn drain_queue(&self) -> Vec<String> {
        self.queue
            .lock()
            .unwrap()
            .drain(..)
            .filter(|(queued_at, _)| queued_at.elapsed() < msg_ttl())
            .map(|(_, msg)| msg)
            .collect()
    }
}

struct AppState {
    channels: DashMap<String, Arc<Channel>>,
    apns: Option<apns::Apns>,
    limiter: ratelimit::RateLimiter,
    stats: Arc<stats::Stats>,
    admin_password: Option<String>,
}

impl AppState {
    /// Touches inside the map-entry lock so the sweeper (which holds the
    /// same shard lock during retain) can never remove a channel between
    /// lookup and use.
    fn channel(&self, id: &str) -> Arc<Channel> {
        let entry = self
            .channels
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(Channel::new()));
        entry.touch();
        entry.clone()
    }
}

async fn listen(
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if id.len() > MAX_ID_LEN {
        return Err(StatusCode::URI_TOO_LONG);
    }
    let guard = state.stats.connect_guard(addr.ip());
    let ch = state.channel(&id);

    // Subscribe before draining the queue so nothing posted in between is lost:
    // once subscribed, receiver_count > 0 and new posts go to the broadcast.
    let (rx, backlog) = {
        let mut queue = ch.queue.lock().unwrap();
        let rx = ch.tx.subscribe();
        let backlog: Vec<String> = queue
            .drain(..)
            .filter(|(queued_at, _)| queued_at.elapsed() < msg_ttl())
            .map(|(_, msg)| msg)
            .collect();
        (rx, backlog)
    };

    // `guard` rides along with the stream; its Drop fires on client disconnect.
    let stream = stream::iter(backlog)
        .chain(BroadcastStream::new(rx).filter_map(|r| async move { r.ok() }))
        .map(move |msg| {
            let _ = &guard;
            Ok(Event::default().data(msg))
        });

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    ))
}

async fn post_message(
    Path(id): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    body: String,
) -> Result<&'static str, (StatusCode, String)> {
    if id.len() > MAX_ID_LEN {
        return Err((StatusCode::URI_TOO_LONG, "ID TOO LONG".to_string()));
    }
    if let Some(token) = id.strip_prefix("apns_") {
        state.stats.record_push(addr.ip(), token);
        // Keep a short recovery copy independently of APNs delivery. A client
        // can drain it later from the token's `/queued` endpoint if the push
        // is delayed or lost.
        state.channel(&id).enqueue(body.clone());
        let apns = state.apns.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "APNS NOT CONFIGURED".to_string(),
        ))?;
        let msg = apns::ApnsMessage::parse(&body);
        return match apns.send(token, &msg).await {
            Ok(()) => Ok("OK"),
            Err(e) => Err((StatusCode::BAD_GATEWAY, format!("APNS ERROR: {e}"))),
        };
    }

    state.stats.record_post(addr.ip(), &id);
    let ch = state.channel(&id);

    // The queue lock serializes with a connecting listener's subscribe+drain,
    // so a message is either delivered live or queued — never dropped in between.
    let mut queue = ch.queue.lock().unwrap();
    match ch.tx.send(body) {
        Ok(_) => Ok("OK"),
        Err(broadcast::error::SendError(msg)) => {
            queue.retain(|(queued_at, _)| queued_at.elapsed() < msg_ttl());
            if queue.len() >= MAX_QUEUE {
                queue.pop_front();
            }
            queue.push_back((Instant::now(), msg));
            Ok("OK QUEUED")
        }
    }
}

fn queued_json_value(body: String) -> serde_json::Value {
    match serde_json::from_str(&body) {
        Ok(value @ serde_json::Value::Object(_)) => value,
        _ => serde_json::Value::String(body),
    }
}

async fn get_queued(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, StatusCode> {
    if id.len() > MAX_ID_LEN {
        return Err(StatusCode::URI_TOO_LONG);
    }
    if !id.starts_with("apns_") {
        return Err(StatusCode::NOT_FOUND);
    }

    let messages: Vec<serde_json::Value> = state
        .channels
        .get(&id)
        .map(|entry| {
            entry.touch();
            entry
                .drain_queue()
                .into_iter()
                .map(queued_json_value)
                .collect()
        })
        .unwrap_or_default();

    Ok(([(header::CACHE_CONTROL, "no-store")], Json(messages)).into_response())
}

/// Compares without early exit; runtime depends only on max(len), not on
/// where the strings differ or whether lengths match.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

fn admin_authorized(state: &AppState, headers: &axum::http::HeaderMap) -> bool {
    use base64::Engine;
    let Some(password) = &state.admin_password else {
        return false;
    };
    let Some(auth) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
    else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(auth) else {
        return false;
    };
    // "user:password" — username is ignored
    let Some(colon) = decoded.iter().position(|&b| b == b':') else {
        return false;
    };
    constant_time_eq(&decoded[colon + 1..], password.as_bytes())
}

fn admin_401() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [("www-authenticate", "Basic realm=\"sidepulse-admin\"")],
        "UNAUTHORIZED",
    )
        .into_response()
}

async fn admin_page(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !state.limiter.check(addr.ip()) {
        return (StatusCode::TOO_MANY_REQUESTS, "RATE LIMITED").into_response();
    }
    if !admin_authorized(&state, &headers) {
        return admin_401();
    }
    axum::response::Html(include_str!("admin.html")).into_response()
}

async fn admin_stats(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !state.limiter.check(addr.ip()) {
        return (StatusCode::TOO_MANY_REQUESTS, "RATE LIMITED").into_response();
    }
    if !admin_authorized(&state, &headers) {
        return admin_401();
    }
    axum::Json(state.stats.snapshot()).into_response()
}

async fn rate_limit(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if !state.limiter.check(addr.ip()) {
        return (StatusCode::TOO_MANY_REQUESTS, "RATE LIMITED").into_response();
    }
    next.run(req).await
}

fn spawn_sweeper(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            // Queued messages can't outlive msg_ttl() and can't be newer than
            // the last touch, so an idle channel's queue is always stale —
            // no separate queue check needed.
            state.channels.retain(|_, ch| {
                ch.tx.receiver_count() > 0
                    || ch.touched.lock().unwrap().elapsed() < IDLE_TTL.max(msg_ttl())
            });
            state.limiter.sweep();
        }
    });
}

async fn acme_challenge(
    State(webroot): State<String>,
    Path(token): Path<String>,
) -> Result<String, StatusCode> {
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(StatusCode::NOT_FOUND);
    }
    tokio::fs::read_to_string(format!("{webroot}/.well-known/acme-challenge/{token}"))
        .await
        .map_err(|_| StatusCode::NOT_FOUND)
}

async fn redirect_https(uri: axum::http::Uri) -> Response {
    // Pinned to the canonical domain: never reflect the Host header
    // (open-redirect vector).
    let domain =
        std::env::var("DOMAIN").unwrap_or_else(|_| "bridge.sidepulse.io".into());
    let target = format!(
        "https://{domain}{}",
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/")
    );
    axum::response::Redirect::permanent(&target).into_response()
}

/// Port-80 listener: serves ACME HTTP-01 challenges (so certbot can renew in
/// webroot mode without stopping anything) and redirects everything else to
/// HTTPS.
fn spawn_http_redirect() {
    let webroot = std::env::var("ACME_WEBROOT")
        .unwrap_or_else(|_| "/var/lib/sidepulse-bridge/acme".into());
    let addr: SocketAddr = std::env::var("HTTP_BIND")
        .unwrap_or_else(|_| "0.0.0.0:80".into())
        .parse()
        .expect("invalid HTTP_BIND address");
    tokio::spawn(async move {
        let app = Router::new()
            .route("/.well-known/acme-challenge/{token}", get(acme_challenge))
            .fallback(redirect_https)
            .with_state(webroot);
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                tracing::info!("HTTP redirect listening on http://{addr}");
                let _ = axum::serve(listener, app).await;
            }
            Err(e) => tracing::error!("HTTP redirect bind failed on {addr}: {e}"),
        }
    });
}

/// Hot-reload the TLS certificate when certbot renews it, so existing
/// SSE connections are never dropped by a restart.
fn spawn_cert_reloader(config: axum_server::tls_rustls::RustlsConfig, cert: String, key: String) {
    fn mtime(path: &str) -> Option<std::time::SystemTime> {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }
    tokio::spawn(async move {
        let mut last = mtime(&cert);
        let mut interval = tokio::time::interval(Duration::from_secs(600));
        interval.tick().await;
        loop {
            interval.tick().await;
            let current = mtime(&cert);
            if current != last {
                last = current;
                match config.reload_from_pem_file(&cert, &key).await {
                    Ok(()) => tracing::info!("TLS certificate reloaded"),
                    Err(e) => tracing::error!("TLS certificate reload failed: {e}"),
                }
            }
        }
    });
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sidepulse_bridge=info".into()),
        )
        .init();

    let apns = match apns::Apns::from_env() {
        Ok(apns) => apns,
        Err(e) => {
            eprintln!("fatal: {e}");
            std::process::exit(1);
        }
    };
    if apns.is_some() {
        tracing::info!("APNS enabled");
    } else {
        tracing::info!("APNS disabled (set APNS_KEY_PATH, APNS_KEY_ID, APNS_TEAM_ID, APNS_TOPIC)");
    }

    let admin_password = std::env::var("ADMIN_PASSWORD").ok().filter(|p| !p.is_empty());
    if admin_password.is_none() {
        tracing::info!("admin UI disabled (set ADMIN_PASSWORD)");
    }

    let state = Arc::new(AppState {
        channels: DashMap::new(),
        apns,
        limiter: ratelimit::RateLimiter::from_env(),
        stats: Arc::new(stats::Stats::new()),
        admin_password,
    });
    spawn_sweeper(state.clone());

    let app = Router::new()
        .route(
            "/",
            get(|| async { axum::response::Html(include_str!("index.html")) }),
        )
        .route("/api/leds/{id}", get(listen).post(post_message))
        .route("/api/leds/{id}/queued", get(get_queued))
        .layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        .route("/healthz", get(|| async { "OK" }))
        .route("/admin", get(admin_page))
        .route("/admin/stats.json", get(admin_stats))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state);

    let cert = std::env::var("TLS_CERT")
        .unwrap_or_else(|_| "/etc/letsencrypt/live/bridge.sidepulse.io/fullchain.pem".into());
    let key = std::env::var("TLS_KEY")
        .unwrap_or_else(|_| "/etc/letsencrypt/live/bridge.sidepulse.io/privkey.pem".into());

    // Distinguish "certs not set up" (dev fallback to HTTP) from "certs exist
    // but are unreadable" (misconfiguration — fail loudly, never silently
    // serve plaintext).
    let tls_ready = match (std::fs::File::open(&cert), std::fs::File::open(&key)) {
        (Ok(_), Ok(_)) => true,
        (Err(e1), Err(e2))
            if e1.kind() == std::io::ErrorKind::NotFound
                && e2.kind() == std::io::ErrorKind::NotFound =>
        {
            false
        }
        (c, k) => {
            eprintln!("fatal: TLS files misconfigured: cert {cert}: {c:?}, key {key}: {k:?}");
            std::process::exit(1);
        }
    };

    if tls_ready {
        let addr: SocketAddr = std::env::var("BIND")
            .unwrap_or_else(|_| "0.0.0.0:443".into())
            .parse()
            .expect("invalid BIND address");
        let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .expect("failed to load TLS certificate");
        spawn_cert_reloader(config.clone(), cert, key);
        spawn_http_redirect();
        tracing::info!("listening on https://{addr}");
        axum_server::bind_rustls(addr, config)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .unwrap();
    } else {
        let addr: SocketAddr = std::env::var("BIND")
            .unwrap_or_else(|_| "0.0.0.0:8080".into())
            .parse()
            .expect("invalid BIND address");
        tracing::info!("TLS certs not found, listening on http://{addr}");
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_queue_keeps_the_latest_five_in_fifo_order() {
        let channel = Channel::new();
        for n in 1..=6 {
            channel.enqueue(format!("message-{n}"));
        }

        assert_eq!(
            channel.drain_queue(),
            vec![
                "message-2",
                "message-3",
                "message-4",
                "message-5",
                "message-6"
            ]
        );
        assert!(channel.drain_queue().is_empty());
    }

    #[test]
    fn recovery_queue_returns_json_objects_and_plain_text() {
        assert_eq!(
            queued_json_value(r#"{"title":"Title","text":"Message"}"#.into()),
            serde_json::json!({"title": "Title", "text": "Message"})
        );
        assert_eq!(
            queued_json_value("plain text".into()),
            serde_json::json!("plain text")
        );
    }
}
