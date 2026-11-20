# Development

Run `make` for all targets.

## Dev stack

Use Docker with Compose 2.32 or later for file watch.

- `make dev` builds and starts the stack, waits for health, then prints
  `https://127.0.0.1:8443`, the LAN signaling URL and the SHA-256 fingerprint
  of its TLS certificate, the server identity fingerprint, and a fresh owner
  invite link while the server is unclaimed.
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
  type `reset`. The next start generates a different TLS certificate and
  server key.

Published ports are 8443/TCP for HTTPS, 7881/TCP for LiveKit media fallback,
and 7882/UDP for LiveKit media. The API's port 8080 and LiveKit's signaling
port 7880 are internal to the stack.

Run the bot once against a chosen address from the repo root:

```sh
cargo run --locked -p rusttalk-bot -- --server wss://192.168.1.10:8443 --identity gate --once
```

The bot hashes `.dev/tls/cert.pem` by default. Pass `--fingerprint` to use
the hex fingerprint from `make dev`, with or without colons. The bot also
checks the application server key and saves its pins in `.dev/bots/servers.json`.
The default
dev credentials match `deploy/livekit.dev.yml`. Set
`LIVEKIT_API_KEY` and `LIVEKIT_API_SECRET` to override them.

On Linux, building the bot requires Clang 21 or later, `pkg-config` and
GLib development headers. The SDK downloads prebuilt WebRTC during the
first build.

## Native client

Run `make client` in the repository root to build and start the native client.
It uses the platform config directory and the OS keystore (Windows Credential
Manager or an unlocked Linux Secret Service session).

Run `make client PROFILE=a` for an isolated identity. A profile is exactly
one lowercase letter from `a` to `z`. For profile `a`, the settings and data
directory is `.dev/profiles/a`, which holds `rusttalk.log` and the startup
lock. The keystore service is `RustTalk-dev-a`. `make reset` removes the
directory and leaves OS keystore entries in place.

On the first successful start, the client prints its recovery words to the
terminal and saves the device identity. Each successful start logs the person
fingerprint to `rusttalk.log`. The client currently opens a console on Windows
so the first-run words are visible there too. Recovery-word confirmation is
available in core for the UI.

## Checks

- `make lint`: clippy with warnings as errors, plus `rustfmt --check`.
- `make fmt`: format all code.
- `make test`: unit and integration tests without Docker.
- `make e2e`: starts the stack detached, tests W05 with two bots, and removes
  the test containers, TLS directory and API volume on exit. Run it when
  the regular dev stack has released ports 8443, 7881 and 7882. It checks
  owner bootstrap, invite use, kick, ban, unban, key pinning and API log
  redaction. E2E runs locally and stays out of CI for now.

Linux CI runs `make lint test` with the bot excluded and `rusttalk-core/livekit-tls`
enabled, so it checks pinning and ICE filtering without the native WebRTC
toolchain and download. Local `make lint test` covers the whole workspace.
Windows CI runs the client unit tests. Both jobs have a 15-minute timeout.
