# Architecture

Components: caddy (TLS edge), api (Rust, axum, SQLite), livekit (unmodified upstream), index (optional), desktop client, bot.

## Repository layout

```
crates/core      identity, crypto, protocol types (shared by client, bot, api)
crates/client    desktop app
crates/bot       headless test client
crates/pipewire  Linux per-stream audio capture helper
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
and on the LAN address. Both connectors select the pin by host and effective
port and refuse unknown origins. A change to the pin at a known origin stops
the join.
[SDK transport hooks](https://github.com/livekit/rust-sdks/blob/livekit/v0.11.0/livekit-net/src/lib.rs#L43-L63).


## Application audio

The core `capture` feature lists and starts local app audio on Linux and
Windows and emits 48 kHz stereo s16 PCM in 10 ms frames. On Windows it calls
WASAPI process loopback from a COM MTA thread. On Linux the desktop client
and bot launch the sibling `rusttalk-pipewire` executable. It uses PipeWire
0.10.1 and system libpipewire. Keeping its dependency graph separate
prevents the weak PipeWire stubs in the prebuilt WebRTC archive from
shadowing libpipewire entry points. The helper reports the running playback
nodes as JSON and sends PCM on a private stdout pipe. It adds a capture
stream targeting the selected PipeWire object serial, with target fallback
and moving disabled. The playback stream keeps its existing output.
