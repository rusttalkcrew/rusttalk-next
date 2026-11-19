# Development

Run `make` for all targets.

## Dev stack

Use Docker with Compose 2.32 or later for file watch.

- `make dev` builds and starts the stack, waits for health, then prints
  `https://127.0.0.1:8443`, the LAN signaling URL and the SHA-256 fingerprint
  of its TLS certificate.
  Leave it running to rebuild the API when `services/api`, `crates/core` or
  root `Cargo.*` files change. Ctrl-C stops the watcher.
- `make dev NODE_IP=192.168.1.10` overrides the IPv4 address that LiveKit
  advertises for media. By default Make reads the source address of the
  host's IPv4 route. Use the override when the selected route uses a VPN
  or when the host has no default route.
- `make bot` starts `bot-a` and `bot-b` in the same dev room. Each logs
  `joined` when its WebRTC data channel opens. Ctrl-C stops the bots.
- `make logs` follows all service logs in another terminal.
- `make down` stops the stack and keeps local data.
- `make reset` stops the stack and deletes its volumes and `.dev/` after you
  type `reset`. The next start generates a different TLS certificate.

Published ports are 8443/TCP for HTTPS, 7881/TCP for LiveKit media fallback,
and 7882/UDP for LiveKit media. The API's port 8080 and LiveKit's signaling
port 7880 are internal to the stack.

Run the bot once against a chosen address from the repo root:

```sh
cargo run --locked -p rusttalk-bot -- --server wss://192.168.1.10:8443 --identity gate --once
```

The bot hashes `.dev/tls/cert.pem` by default. Pass `--fingerprint` to use
the hex fingerprint from `make dev`, with or without colons. The default
dev credentials match `deploy/livekit.dev.yml`. Set
`LIVEKIT_API_KEY` and `LIVEKIT_API_SECRET` to override them.

On Linux, building the bot requires Clang 21 or later, `pkg-config` and
GLib development headers. The SDK downloads prebuilt WebRTC during the
first build.

## Checks

- `make lint`: clippy with warnings as errors, plus `rustfmt --check`.
- `make fmt`: format all code.
- `make test`: unit and integration tests.

Linux CI runs `make lint test` with the bot excluded and `rusttalk-core/livekit-tls`
enabled, so it checks pinning and ICE filtering without the native WebRTC
toolchain and download. Local `make lint test` covers the whole workspace.
Windows CI runs the client unit tests. Both jobs have a 15-minute timeout.
