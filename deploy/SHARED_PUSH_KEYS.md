# Shared push key deployment

Deployed to `bridge.sidepulse.io` on September 29, 2026 (America/Los_Angeles).
The service started at 23:21:53 PDT, September 29 (06:21:53 UTC, September 30).

## Installed artifact

- Service: `sidepulse-bridge`
- Binary: `/opt/sidepulse-bridge/sidepulse-bridge`
- SHA-256: `1b083bd9e524e1305277a47cb0e8e95271dd86c4d85e8a70e38f0a88229a3241`
- Previous binary: `/opt/sidepulse-bridge/sidepulse-bridge.before-shared-keys-20260929`
- Build: Linux x86_64, release profile, `cargo build --release --locked` in the
  `rust:1` Docker image using the repository's `Cargo.lock`.

The binary was staged, checked against the local SHA-256, installed atomically,
and the service restarted. Existing APNs credentials, environment configuration,
TLS certificates, and service definition were retained. Recovery queues are in
memory and do not survive a service restart.

## Compatibility

Both existing token forms remain supported by the server:

```text
<device-token>
dev_<device-token>
```

Keyed forms add an optional, case-sensitive suffix:

```text
<device-token>_<shared-key>
dev_<device-token>_<shared-key>
```

The bridge strips routing metadata before sending to Apple. Only keyed requests
add authoritative top-level `shared_key` and generated `sidepulse_push_id` fields
to APNs and recovery copies. Body-supplied values cannot override these fields.
Unkeyed plain-text recovery retains its existing string format.

There is no server allowlist or requirement to supply a key. The updated iOS app
independently rejects pushes whose keys are missing, unknown, or removed. Thus
server compatibility for legacy clients does not authorize unkeyed messages in
the updated app. See [the API policy](../API.md#sidepulse-sender-authorization-and-notification-cleanup).

## Validation

- All 16 Rust tests passed; Clippy passed with warnings treated as errors.
- Public HTTPS health check returned `OK`; systemd reported active with no
  automatic restarts after deployment.
- Production and development routes were checked with synthetic device tokens:
  Apple rejected the nonexistent device rather than the bridge rejecting an
  unkeyed token. Legacy plain-text recovery was unchanged.
- Keyed JSON recovery preserved the URL's key and generated message ID,
  overriding spoofed body fields. Malformed suffixes returned `400` without
  replacing the existing queued message. Keyed and unkeyed recovery URLs drained
  the same device queue.
- Real development-token requests, both keyed and unkeyed, returned `200 OK`
  through the live bridge. Recovery metadata was verified. The test keys were
  not active in the phone app; it logged both rejections and left its inbox
  unchanged. Test payloads contained no LED commands or visible alerts.

The iOS development build is installed on the test phone. The desktop CLI was
subsequently installed into `~/.local/share/sidepulse/venv`, retaining the
`~/.local/bin/sidepulse` command. Its installed version is
`1.dev65+g2f8e6af22.d20260930`. All 37 link tests passed. The installed module was
checked against the source, and runtime checks verified key-case preservation,
legacy-token support, and replacement of a saved link on re-pairing. Dependency
and command-startup checks passed. Any already-running CLI process must restart
to load the new code. After installation, the user confirmed that the updated
CLI, bridge, and app worked together. The automated live checks above covered
APNs acceptance, recovery metadata, and rejection of inactive keys.

## Rollback

On the VM, restore the saved binary atomically and restart the service:

```sh
sudo install -m 755 \
  /opt/sidepulse-bridge/sidepulse-bridge.before-shared-keys-20260929 \
  /opt/sidepulse-bridge/sidepulse-bridge.rollback
sudo mv /opt/sidepulse-bridge/sidepulse-bridge.rollback \
  /opt/sidepulse-bridge/sidepulse-bridge
sudo systemctl restart sidepulse-bridge
curl --fail https://bridge.sidepulse.io/healthz
```

Rolling back removes shared-key support from the server; the updated iOS app
will reject resulting unkeyed messages.
