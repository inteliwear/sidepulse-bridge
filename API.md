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
  is forwarded to Apple Push Notification service instead of being buffered.

## Endpoints

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

`{device_token}` is the hex APNs device token from the iOS app. Body is
either plain text, or JSON:

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

Delivery mode: if `title`, `text`, or `alert` is set, the push is a visible
notification (push-type `alert`, priority 10, default sound). Otherwise it is
a **silent background push** (`aps: {"content-available": 1}`, push-type
`background`, priority 5) carrying just the custom keys — the normal mode for
LED updates.

Responses:

| Status | Body                    | Meaning                              |
|--------|-------------------------|--------------------------------------|
| `200`  | `OK`                    | Accepted by APNs.                    |
| `502`  | `APNS ERROR: <reason>`  | APNs rejected it (bad token, etc.).  |
| `503`  | `APNS NOT CONFIGURED`   | Server has no APNs credentials.      |

```sh
curl -X POST -d '{"leds":"HELLO","title":"SidePulse","text":"New message"}' \
  https://bridge.sidepulse.io/api/leds/apns_a1b2c3d4e5f6...
```

### 4. Health — `GET /healthz`

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
