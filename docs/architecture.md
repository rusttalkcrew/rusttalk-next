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
7881/TCP fallback. Media uses WebRTC DTLS-SRTP with the fingerprint negotiated
in signaling.

On IP deployments, LiveKit Rust SDK 0.11.0 uses the shared
`rusttalk-core/livekit-tls` transport. The first voice join registers both
the WebSocket connector and the HTTP connector used for `/rtc/validate`.
Rustls verifies the SHA-256 fingerprint of the exact leaf certificate over
DER and checks the handshake signatures. The pin replaces CA and hostname
validation for this connection, so the same certificate works on loopback
and on the LAN address. A missing or different pin stops the join. The SDK
registration covers one signaling origin per process.
[SDK transport hooks](https://github.com/livekit/rust-sdks/blob/livekit/v0.11.0/livekit-net/src/lib.rs#L43-L63).
