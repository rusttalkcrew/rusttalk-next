# Architecture

Components: caddy (TLS edge), api (Rust, axum, SQLite), livekit (unmodified upstream), index (optional), desktop client, bot.

## Repository layout

```
crates/core      identity, crypto, protocol types (shared by client, bot, api)
crates/client    desktop app
crates/bot       headless test client
services/api     server
services/index   directory service
deploy/          compose files, caddy and livekit config
docs/            this documentation
```

## Request routing

Caddy terminates TLS. It sends `/v1/*` to the API on port 8080 and `/rtc*`
to LiveKit signaling on port 7880, including WebSocket upgrades. LiveKit
media bypasses Caddy and goes directly to LiveKit on port 7882/UDP or the
7881/TCP fallback.
