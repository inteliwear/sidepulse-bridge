use std::{
    collections::VecDeque,
    net::IpAddr,
    sync::{
        atomic::{AtomicI64, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use dashmap::DashMap;
use serde_json::json;

/// Minutes of history kept for the graph (24 h).
const HISTORY: usize = 1440;
/// Cap on per-day distinct keys tracked, to bound memory.
const MAX_KEYS: usize = 100_000;

#[derive(Default)]
struct Minute {
    min: u64,
    posts: u32,
    pushes: u32,
    connects: u32,
}

#[derive(Default)]
struct Counts {
    posts: u64,
    pushes: u64,
    connects: u64,
}

/// Aggregate counters only — message bodies are never stored.
pub struct Stats {
    sse_active: AtomicI64,
    total_posts: AtomicU64,
    total_pushes: AtomicU64,
    total_connects: AtomicU64,
    day: Mutex<u64>,
    per_ip: DashMap<IpAddr, Counts>,
    per_channel: DashMap<String, u64>,
    per_token: DashMap<String, u64>,
    minutes: Mutex<VecDeque<Minute>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

impl Stats {
    pub fn new() -> Self {
        Self {
            sse_active: AtomicI64::new(0),
            total_posts: AtomicU64::new(0),
            total_pushes: AtomicU64::new(0),
            total_connects: AtomicU64::new(0),
            day: Mutex::new(now_secs() / 86_400),
            per_ip: DashMap::new(),
            per_channel: DashMap::new(),
            per_token: DashMap::new(),
            minutes: Mutex::new(VecDeque::with_capacity(HISTORY)),
        }
    }

    /// Reset the per-day maps at midnight UTC.
    fn roll_day(&self) {
        let today = now_secs() / 86_400;
        let mut day = self.day.lock().unwrap();
        if *day != today {
            *day = today;
            self.per_ip.clear();
            self.per_channel.clear();
            self.per_token.clear();
        }
    }

    fn bump_minute(&self, f: impl Fn(&mut Minute)) {
        let min = now_secs() / 60;
        let mut minutes = self.minutes.lock().unwrap();
        if minutes.back().map(|m| m.min) != Some(min) {
            if minutes.len() >= HISTORY {
                minutes.pop_front();
            }
            minutes.push_back(Minute {
                min,
                ..Default::default()
            });
        }
        f(minutes.back_mut().unwrap());
    }

    pub fn record_post(&self, ip: IpAddr, channel: &str) {
        self.roll_day();
        self.total_posts.fetch_add(1, Ordering::Relaxed);
        self.bump_minute(|m| m.posts += 1);
        if self.per_ip.len() < MAX_KEYS || self.per_ip.contains_key(&ip) {
            self.per_ip.entry(ip).or_default().posts += 1;
        }
        if self.per_channel.len() < MAX_KEYS || self.per_channel.contains_key(channel) {
            *self.per_channel.entry(channel.to_string()).or_default() += 1;
        }
    }

    pub fn record_push(&self, ip: IpAddr, token: &str) {
        self.roll_day();
        self.total_pushes.fetch_add(1, Ordering::Relaxed);
        self.bump_minute(|m| m.pushes += 1);
        if self.per_ip.len() < MAX_KEYS || self.per_ip.contains_key(&ip) {
            self.per_ip.entry(ip).or_default().pushes += 1;
        }
        if self.per_token.len() < MAX_KEYS || self.per_token.contains_key(token) {
            *self.per_token.entry(token.to_string()).or_default() += 1;
        }
    }

    /// Counts an SSE connect and returns a guard that tracks the live
    /// connection; drop (= client disconnect) decrements the gauge.
    pub fn connect_guard(self: &Arc<Self>, ip: IpAddr) -> ConnectionGuard {
        self.roll_day();
        self.total_connects.fetch_add(1, Ordering::Relaxed);
        self.bump_minute(|m| m.connects += 1);
        if self.per_ip.len() < MAX_KEYS || self.per_ip.contains_key(&ip) {
            self.per_ip.entry(ip).or_default().connects += 1;
        }
        self.sse_active.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard(self.clone())
    }

    pub fn snapshot(&self) -> serde_json::Value {
        self.roll_day();

        fn top<K, V>(
            map: &DashMap<K, V>,
            n: usize,
            fmt: impl Fn(&K, V) -> serde_json::Value,
        ) -> Vec<serde_json::Value>
        where
            K: Clone + std::hash::Hash + Eq,
            V: Copy + Ord,
        {
            let mut rows: Vec<_> = map.iter().map(|e| (e.key().clone(), *e.value())).collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1));
            rows.truncate(n);
            rows.iter().map(|(k, v)| fmt(k, *v)).collect()
        }

        let minutes = self.minutes.lock().unwrap();
        let now_min = now_secs() / 60;
        // Dense last-24h series, zero-filled where no bucket exists.
        let start = now_min + 1 - HISTORY as u64;
        let (mut posts, mut pushes, mut connects) =
            (vec![0u32; HISTORY], vec![0u32; HISTORY], vec![0u32; HISTORY]);
        for m in minutes.iter() {
            if m.min >= start {
                let i = (m.min - start) as usize;
                // Upper bound guards against a backward clock step.
                if i < HISTORY {
                    posts[i] = m.posts;
                    pushes[i] = m.pushes;
                    connects[i] = m.connects;
                }
            }
        }

        // Identities (IPs, channel IDs, push tokens) never leave the server —
        // only ranked, anonymous counts.
        let per_ip_top: Vec<_> = {
            let mut rows: Vec<_> = self
                .per_ip
                .iter()
                .map(|e| (e.value().posts, e.value().pushes, e.value().connects))
                .collect();
            rows.sort_by(|a, b| (b.0 + b.1 + b.2).cmp(&(a.0 + a.1 + a.2)));
            rows.truncate(10);
            rows.iter()
                .map(|(p, pu, c)| json!({"posts": p, "pushes": pu, "connects": c}))
                .collect()
        };

        json!({
            "sse_active": self.sse_active.load(Ordering::Relaxed),
            "totals": {
                "posts": self.total_posts.load(Ordering::Relaxed),
                "pushes": self.total_pushes.load(Ordering::Relaxed),
                "connects": self.total_connects.load(Ordering::Relaxed),
            },
            "today": {
                "unique_ips": self.per_ip.len(),
                "unique_channels": self.per_channel.len(),
                "unique_push_tokens": self.per_token.len(),
            },
            "top_ips": per_ip_top,
            "top_channels": top(&self.per_channel, 10, |_, v| json!({"posts": v})),
            "top_tokens": top(&self.per_token, 10, |_, v| json!({"pushes": v})),
            "minutes": { "start_min": start, "posts": posts, "pushes": pushes, "connects": connects },
        })
    }
}

pub struct ConnectionGuard(Arc<Stats>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.sse_active.fetch_sub(1, Ordering::Relaxed);
    }
}
