# SidePulse Bridge — API Specification

Minimal message-passing API over HTTPS + Server-Sent Events. This document is
self-contained: everything needed to build a client in any language is here.

**Base URL:** `https://bridge.sidepulse.io`

## Concepts

- A **channel** is identified by an arbitrary ID in the URL path — by
  convention a randomly generated UUID. Channels are created implicitly on
  first use; there is no registration.
- One side **listens** (GET, SSE stream); the other side **posts** (POST,
  plain text body).
- If no listener is connected, the channel buffers up to **5 messages**
  (FIFO). When full, the **oldest** message is dropped. A listener receives
  buffered messages immediately on connect. Buffered messages expire after
  **5 minutes** — the buffer exists to cover dropped-connection recovery,
  not offline storage.
- Channel IDs starting with `apns_` are **push-notification channels**: a POST
  is forwarded to Apple Push Notification service and retained for recovery.

## Endpoints

### Agent instructions — `GET /agents`

Response: `200`, `Content-Type: text/plain; charset=utf-8`,
`Cache-Control: no-store`. Serves Markdown instructions as plain text so browsers can display them
inline. The instructions cover a SidePulse Dot connected to an iPhone.

Share `https://bridge.sidepulse.io/agents#apns_<copied-token>` with an agent.
The agent must extract the complete channel ID after `#` from the original
user-supplied link, including any environment prefix and sender-key suffix,
then POST to `/api/leds/<channel-id>`. URL fragments are not sent in HTTP
requests, so the returned Markdown is device-independent. This endpoint
does not create channels, send pushes, or drain queues.

### 1. Listen — `GET /api/leds/{id}`

Response: `200`, `Content-Type: text/event-stream`. Standard SSE: each message
arrives as an event whose `data:` lines carry the message text. A keep-alive
comment line (`: keep-alive`) is sent every 15 s; ignore lines starting with `:`.

```
data: hello world

data: second message

```

Parsing rules (standard SSE): a message is the concatenation of consecutive
`data:` lines (joined with `\n`), terminated by a blank line. Reconnect on
disconnect; messages posted while disconnected are buffered (up to 5).

```sh
curl -N https://bridge.sidepulse.io/api/leds/6f1c2a9e-8f4b-4c1d-9b3a-2e5d7c8f0a11
```

### 2. Send — `POST /api/leds/{id}`

Request body: the message as plain text (UTF-8, max 64 KB). No headers
required.

Responses, `200` with a plain-text body:

| Body        | Meaning                                                        |
|-------------|----------------------------------------------------------------|
| `OK`        | A listener was connected and received the message immediately. |
| `OK QUEUED` | No listener connected; message buffered (oldest of 5 dropped if full). |

```sh
curl -X POST -d 'hello world' https://bridge.sidepulse.io/api/leds/6f1c2a9e-8f4b-4c1d-9b3a-2e5d7c8f0a11
```

### 3. Push notification — `POST /api/leds/apns_{device_token}`

`{device_token}` is the hex APNs device token from the iOS app, optionally
prefixed with `dev_`:

- `apns_{hex_token}` routes to production APNs (TestFlight / App Store).
- `apns_dev_{hex_token}` routes to sandbox APNs (development builds). The
  bridge removes `dev_` before passing the device token to Apple.

An optional shared key is appended after the hex token with `_`:

- Production: `apns_{hex_token}_{shared_key}`
- Development: `apns_dev_{hex_token}_{shared_key}`

Keys are case-sensitive, 1–128 ASCII letters, digits, underscores, or hyphens
(`A–Z`, `a–z`, `0–9`, `_`, `-`). An empty or malformed suffix returns
`400 INVALID SHARED KEY` before delivery or queueing. The bridge strips both
the environment prefix and key suffix before sending the device token to Apple.
Legacy tokens without a key continue to work and omit `shared_key`.
Keyed pushes also include a bridge-generated top-level `sidepulse_push_id`
(UUID), identical in APNs and recovery copies. Apps can use it to count a
message once across delivery paths. A body-supplied `sidepulse_push_id` cannot
override it.

The key is delivered as the top-level custom payload field `shared_key`, for
both plain-text and JSON requests. This field is reserved: a field with that
name in the request body cannot supply or override it. For example, posting
`HELLO` to `apns_{hex_token}_sender-key` produces custom data:

```json
{"leds":"HELLO","shared_key":"sender-key","sidepulse_push_id":"<bridge-generated UUID>"}
```

The app should generate a random secret key for each sender, share the suffixed
token with that sender, and match incoming `shared_key` values against its local
list before processing LED updates. That lets it label senders and reject
unknown, missing, or revoked keys according to its policy. Anyone holding the
same key has the same sender identity. The bridge forwards keys; it does not
register, authenticate, or revoke them. This requires app-side validation for
both APNs and recovery messages.

#### SidePulse sender authorization and notification cleanup

The SidePulse iOS app issues a separate random key when pairing a sender or
using **Copy New Token**. **Copy Token** on an existing key reuses it. The CLI
stores the entire token, including `dev_` when present and the case-sensitive
key suffix, and posts to the corresponding `apns_` channel. Re-pairing the same
device and APNs environment replaces its saved CLI link with the new token.
The bridge forwards the URL's key and generated message ID in both APNs and
recovery copies; it has no list of active or removed keys.

The app compares the top-level `shared_key` with its locally saved active
keys before processing either delivery path. Nested keys do not authorize a
push. Its behavior is:

| Incoming key | App behavior |
|---|---|
| Matches an active key | Process the payload and record sender activity. Clear a matching LED notification after a successful write. |
| Missing, unknown, or removed | Do not write LEDs, add an inbox entry, change link state, or record sender activity. Dismiss the notification. |

Settings shows each active key's last four characters, last accepted activity
time, and lifetime received count. Full keys stay out of the displayed list
and are masked in payload summaries. Event IDs, or `sidepulse_push_id` when
there is no event ID, prevent repeat callbacks and recovery from counting the
same message twice within the latest 256 IDs retained per sender. Messages
without an ID count on each receipt.

Removing a key immediately revokes it locally and clears delivered
notifications without an active key. A later push cannot restore or enroll
the key; the sender must obtain a newly issued token to resume.

Foreground unauthorized alerts are suppressed. Delivered unauthorized alerts
are removed when the app handles a notification, becomes active, or runs
**Update from Server**. iOS may show a background alert before giving the app
execution time; server acceptance does not mean app authorization.

**Update from Server** first clears unauthorized notifications. With no active
keys it returns without a server fetch. Otherwise it recovers pending messages
and applies the same key checks. An empty queue or rejected payload requires
no update. The action finishes silently; setup and network failures remain in
diagnostics instead of creating Shortcuts error alerts. A saved desktop link
is not required. Local Shortcuts and manual LED writes do not require a remote
sender key.

Deploy the updated bridge and distribute the updated CLI together with the
updated iOS app. The bridge still supports unkeyed requests for legacy clients,
but the updated app rejects them. Older bridge versions do not provide the
required key metadata.

Routing is per token; the legacy `APNS_SANDBOX` / `APNS_ENV` server settings
are ignored. Body is either plain text, or JSON:

```json
{"leds": "LED TEXT", "title": "Alert title", "text": "Alert body",
 "pattern": "pattern-name", "data": {"any": "extra"}}
```

- `leds` — delivered inside the push payload as custom key `leds`
  (aliases: `LEDS.txt`, `LEDS.TXT`).
- `title`, `text` — the notification alert title and body. `alert` is also
  accepted as the body (title then defaults to "SidePulse").
- `pattern`, `data` — optional custom keys passed through in the payload.
- All fields optional. Plain-text body ≡ `{"leds": "<body>"}`.

Delivery mode: every push includes `content-available: 1` so iOS can wake the
app to process custom data. If `title`, `text`, or `alert` is set, the push is
also a visible notification (push-type `alert`, priority 10, default sound).
Otherwise it is a **silent background push** (push-type `background`, priority
5) carrying just the custom keys — the normal mode for LED updates. Background
execution is scheduled by iOS and is not guaranteed.

Responses:

| Status | Body                    | Meaning                              |
|--------|-------------------------|--------------------------------------|
| `200`  | `OK`                    | Accepted by APNs.                    |
| `400`  | `INVALID SHARED KEY` / `EMPTY APNS TOKEN` | Malformed push token; nothing queued. |
| `502`  | `APNS ERROR: <reason>`  | APNs rejected it (bad token, etc.).  |
| `503`  | `APNS NOT CONFIGURED`   | Server has no APNs credentials.      |

All pushes automatically use `apns-collapse-id: sidepulse-led-status`. Successive
notifications for the same app/device merge into one notification; no additional
request field is needed. This does not undo LED commands already processed or
guarantee delivery order.

Every valid APNs post replaces the previous message in its device's recovery queue,
whether APNs accepts or rejects the delivery. The queue keeps only the latest
message for up to 5 minutes. Ordinary SSE channels retain their five-message
buffer.

```sh
curl -X POST -d '{"leds":"HELLO","title":"SidePulse","text":"New message"}' \
  https://bridge.sidepulse.io/api/leds/apns_a1b2c3d4e5f6...
```

### 4. Recover queued pushes — `GET /api/leds/apns_{device_token}/queued`

Returns and drains the recovery queue for that token as a JSON array containing
zero or one messages (the latest unexpired push).
Use the same token including any `dev_` prefix as in the POST URL; production
and development queues are separate even if their hex tokens match.
Shared-key suffixes are ignored when selecting the queue: the app can use
`apns_{hex_token}/queued` (or `apns_dev_{hex_token}/queued`) to recover the latest
message from any sender. A suffixed recovery URL drains that same queue and
does not filter by sender. All senders share the device's latest-message slot,
matching APNs collapse behavior.

JSON request bodies are returned as objects with the URL's `shared_key` added
when present. Keyed plain-text requests are returned as
`{"leds":"<body>","shared_key":"<key>"}`; unkeyed plain-text requests remain
strings. A body-supplied top-level `shared_key` is removed or replaced by the
URL's key. Keyed objects also contain the generated `sidepulse_push_id` from
the APNs copy. A second GET returns an empty array unless new pushes have arrived.

```json
[
  {"leds":"HELLO","title":"SidePulse","text":"New message"}
]
```

```sh
curl https://bridge.sidepulse.io/api/leds/apns_a1b2c3d4e5f6.../queued
```

### 5. Health — `GET /healthz`

Returns `200 OK` with body `OK`. Not rate-limited.

## Rate limits (per client IP)

100 requests/second, 1 000/minute, 100 000/day. Exceeding any window returns
`429` with body `RATE LIMITED`; retry after the window passes. An open SSE
stream counts as one request (at connect time only).

## Errors (all endpoints)

| Status | Meaning                                    |
|--------|--------------------------------------------|
| `400`  | Body is not valid UTF-8.                   |
| `413`  | Body larger than 64 KB.                    |
| `429`  | Rate limited (see above).                  |

## Client recipes

Send and confirm delivery:

```sh
resp=$(curl -s -X POST -d "text" "https://bridge.sidepulse.io/api/leds/$ID")
# "OK" = delivered live, "OK QUEUED" = buffered for later
```

Robust listener (auto-reconnect):

```sh
while true; do
  curl -sN "https://bridge.sidepulse.io/api/leds/$ID" | \
    grep --line-buffered '^data: ' | cut -c7-
  sleep 1
done
```

Python listener (no dependencies beyond `requests` + `sseclient`, or raw):

```python
import requests
with requests.get(f"https://bridge.sidepulse.io/api/leds/{ID}", stream=True) as r:
    for line in r.iter_lines(decode_unicode=True):
        if line and line.startswith("data: "):
            print(line[6:])
```
