# SidePulse Bridge

High-performance message-passing bridge over HTTPS + Server-Sent Events (SSE),
with optional Apple Push Notification (APNs) delivery. Written in Rust
(axum + tokio), designed to run on `https://bridge.sidepulse.io`.

One side **listens** on a randomly generated UUID channel via SSE; the other
side **posts** plain-text messages to the same UUID. If no listener is
connected, up to **5 messages** are queued per channel (oldest dropped when
full) for at most **5 minutes** — the queue covers dropped-connection
recovery, not offline storage.

Full machine-readable API spec (for building clients in any language):
**[API.md](API.md)**.

## CLI

`cli/sidepulse` is a dependency-free shell CLI (just needs `curl`):

```sh
export SIDEPULSE_SERVER=https://bridge.sidepulse.io   # default
id=$(cli/sidepulse gen)          # new random channel UUID
cli/sidepulse listen "$id"       # prints messages as they arrive
cli/sidepulse send "$id" "hello" # -> OK or OK QUEUED
cli/sidepulse notify <apns-token> "Title" "Body" "LED TEXT"
```

## API

### Listen (SSE)

```
GET /api/leds/{UUID}
```

Opens a Server-Sent Events stream. Any queued messages are delivered
immediately, then messages arrive live as they are posted. A keep-alive
comment is sent every 15 seconds.

```sh
curl -N https://bridge.sidepulse.io/api/leds/6f1c2a9e-8f4b-4c1d-9b3a-2e5d7c8f0a11
```

### Post

```
POST /api/leds/{UUID}
Body: plain text (max 64 KB)
```

Responses (plain text):

| Response    | Meaning                                                      |
|-------------|--------------------------------------------------------------|
| `OK`        | A listener was connected; message delivered live.            |
| `OK QUEUED` | No listener; message queued (max 5, oldest dropped if full). |

```sh
curl -d 'hello world' https://bridge.sidepulse.io/api/leds/6f1c2a9e-8f4b-4c1d-9b3a-2e5d7c8f0a11
```

### Apple Push Notifications

Post to a channel whose ID is `apns_` followed by the device push token.
Instead of being queued, the message is sent as a push notification through
APNs.

```
POST /api/leds/apns_{device-push-token}
```

The body is either plain TXT, or JSON:

```json
{ "leds": "out text", "title": "Notification title", "text": "Notification body" }
```

- `leds` — the LED text, included in the push payload as custom data `leds`.
- `title` / `text` — the notification alert title and body. With a plain TXT
  body, the text is used as both `leds` and the alert body.

All pushes include `content-available: 1`, so a visible notification can also
wake the app to process its custom data. Background execution remains subject
to iOS scheduling and is not guaranteed.

Responds `OK` on success, `502` with the APNs error otherwise, and
`503 APNS NOT CONFIGURED` if the server has no APNs credentials.

Every push attempt is also kept in a per-token recovery queue (latest 5, up
to 5 minutes). Fetching the queue drains it:

```sh
curl https://bridge.sidepulse.io/api/leds/apns_<device-token>/queued
```

```sh
curl -d '{"leds":"HELLO","title":"SidePulse","text":"New message"}' \
  https://bridge.sidepulse.io/api/leds/apns_<device-token>
```

### Health check

```
GET /healthz  →  OK
```

### Admin stats UI

`GET /admin` — password-protected (HTTP Basic auth, any username, password
from `ADMIN_PASSWORD`; disabled when unset). Shows aggregate numbers only —
no message content and no identities (IPs, channel IDs, and push tokens never
leave the server): live SSE listener count, posts/pushes/connects per minute
(1 h / 3 h / 24 h graph), unique IPs / channels / push tokens today, and
anonymous ranked top-10 activity tables. JSON at `GET /admin/stats.json`.

### Rate limits

Per client IP (IPv6: per /64): **100/s, 1 000/min, 100 000/day**. Exceeding
any window returns `429 RATE LIMITED`. An SSE stream counts once, at connect
time. Tunable via `RATE_PER_SEC`, `RATE_PER_MIN`, `RATE_PER_DAY`.

> Note: clients behind one NAT (office/coworking space) share these limits —
> raise the sustained tiers via env vars if that becomes an issue.

## Configuration (environment variables)

| Variable         | Default                                                    | Purpose                          |
|------------------|------------------------------------------------------------|----------------------------------|
| `BIND`           | `0.0.0.0:443` (TLS) / `0.0.0.0:8080` (no certs)            | Listen address                   |
| `TLS_CERT`       | `/etc/letsencrypt/live/bridge.sidepulse.io/fullchain.pem`  | TLS certificate chain            |
| `TLS_KEY`        | `/etc/letsencrypt/live/bridge.sidepulse.io/privkey.pem`    | TLS private key                  |
| `APNS_KEY_PATH`  | *(unset = APNs disabled)*                                  | Path to the `.p8` APNs auth key  |
| `APNS_KEY_ID`    | —                                                          | Key ID of the `.p8` key          |
| `APNS_TEAM_ID`   | —                                                          | Apple Developer Team ID          |
| `APNS_TOPIC`     | —                                                          | App bundle ID (apns-topic)       |
| `APNS_SANDBOX`   | unset (production)                                         | Set `1` to use the APNs sandbox  |
| `ADMIN_PASSWORD` | *(unset = admin UI disabled)*                              | Password for `/admin` stats UI   |
| `RATE_PER_SEC` / `RATE_PER_MIN` / `RATE_PER_DAY` | 100 / 1000 / 100000        | Per-IP rate limits               |
| `RUST_LOG`       | `sidepulse_bridge=info`                                    | Log filter                       |

If the TLS cert files don't exist, the server falls back to plain HTTP —
handy for local development.

> **The APNs `.p8` key must never be committed.** It's covered by
> `.gitignore` (`*.p8`); keep it on the server only, e.g. in
> `/etc/sidepulse-bridge/AuthKey_XXXXXXXXXX.p8`.

## Local development

```sh
cargo run
# in one terminal:
curl -N localhost:8080/api/leds/test-uuid
# in another:
curl -d 'hi' localhost:8080/api/leds/test-uuid
```

## Deploying to Google Compute Engine

1. **Create the VM and open the firewall** (a small instance is plenty):

   ```sh
   gcloud compute instances create sidepulse-bridge \
     --machine-type=e2-micro --image-family=debian-12 --image-project=debian-cloud \
     --tags=https-server,http-server
   gcloud compute firewall-rules create allow-http-https \
     --allow=tcp:80,tcp:443 --target-tags=https-server,http-server
   ```

2. **Point DNS**: create an `A` record for `bridge.sidepulse.io` at the VM's
   external IP (reserve a static IP so it survives restarts).

3. **Build the Linux binary in Docker** (keeps the VM clean — no toolchain
   on the server) and copy it over:

   ```sh
   docker run --rm --platform linux/amd64 \
     -v "$PWD":/src -w /src \
     -v sidepulse-cargo-registry:/usr/local/cargo/registry \
     -e CARGO_TARGET_DIR=/src/target-linux \
     rust:1 cargo build --release
   gcloud compute scp target-linux/release/sidepulse-bridge sidepulse-bridge:/tmp/
   ```

4. **On the VM** — install the binary, user, and APNs key:

   ```sh
   sudo useradd --system --no-create-home sidepulse
   sudo mkdir -p /opt/sidepulse-bridge /etc/sidepulse-bridge
   sudo mv /tmp/sidepulse-bridge /opt/sidepulse-bridge/
   # copy your AuthKey_XXXXXXXXXX.p8 into /etc/sidepulse-bridge/ (scp it; never commit it)
   sudo tee /etc/sidepulse-bridge/env >/dev/null <<'EOF'
   APNS_KEY_PATH=/etc/sidepulse-bridge/AuthKey_XXXXXXXXXX.p8
   APNS_KEY_ID=XXXXXXXXXX
   APNS_TEAM_ID=YYYYYYYYYY
   APNS_TOPIC=io.sidepulse.ios
   EOF
   sudo chmod 600 /etc/sidepulse-bridge/env /etc/sidepulse-bridge/*.p8
   sudo chown -R sidepulse:sidepulse /etc/sidepulse-bridge
   ```

5. **Let's Encrypt certificate** — first issuance uses standalone mode
   (before the service is running); renewals then switch to webroot mode,
   served by the bridge's own port-80 listener, so nothing ever has to stop:

   ```sh
   sudo apt-get install -y certbot
   sudo certbot certonly --standalone -d bridge.sidepulse.io
   # let the service user read the certs
   sudo chgrp -R sidepulse /etc/letsencrypt/live /etc/letsencrypt/archive
   sudo chmod -R g+rx /etc/letsencrypt/live /etc/letsencrypt/archive
   # renewals via the bridge's ACME webroot (no port conflict with the service)
   sudo mkdir -p /var/lib/sidepulse-bridge/acme
   sudo certbot reconfigure --cert-name bridge.sidepulse.io \
     -a webroot -w /var/lib/sidepulse-bridge/acme
   ```

   Auto-renewal: certbot's systemd timer checks twice a day. To renew every
   ~2 weeks (rather than certbot's default 30-days-before-expiry), set in
   `/etc/letsencrypt/renewal/bridge.sidepulse.io.conf`:

   ```
   renew_before_expiry = 76 days
   ```

   No restart hook is needed: the bridge watches the cert file and
   **hot-reloads** renewed certificates in-process (checked every 10 min), so
   open SSE connections are never dropped by a renewal.

6. **Install and start the systemd service**:

   ```sh
   sudo cp deploy/sidepulse-bridge.service /etc/systemd/system/
   sudo systemctl enable --now sidepulse-bridge
   curl https://bridge.sidepulse.io/healthz   # → OK
   ```

## Design notes

- **Concurrency**: sharded lock-free map (`dashmap`) of channels; each channel
  is a tokio broadcast plus a tiny mutex-guarded queue. Posting to a live
  listener is a single lock-free-ish broadcast send — no allocation beyond the
  message itself.
- **Delivery**: a connecting listener subscribes *before* draining the queue,
  and posting holds the queue lock while trying the live send, so a message is
  either delivered live (`OK`) or queued (`OK QUEUED`) — never lost in the
  handoff window.
- **Memory**: channels with no listener, an empty queue, and 5 minutes of
  inactivity are swept every 60 s, so random UUIDs don't accumulate.
- **TLS**: rustls in-process (no reverse proxy needed); HTTP/2 and HTTP/1.1
  via ALPN.

## License

GPL-3.0-or-later — see [LICENSE](LICENSE). © 2026 InteliWEAR LLC.
