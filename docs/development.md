# Development

Run `make` for all targets.

## Dev stack

Use Docker with Compose 2.32 or later for file watch.

- `make dev` builds and starts the stack, waits for health, then prints
  `https://127.0.0.1:8443` and the SHA-256 fingerprint of its TLS certificate.
  Leave it running to rebuild the API when `services/api`, `crates/core` or
  root `Cargo.*` files change. Ctrl-C stops the watcher.
- `make logs` follows all service logs in another terminal.
- `make down` stops the stack and keeps local data.
- `make reset` stops the stack and deletes its volumes and `.dev/` after you
  type `reset`. The next start generates a different TLS certificate.

Published ports are 8443/TCP for HTTPS, 7881/TCP for LiveKit media fallback,
and 7882/UDP for LiveKit media. The API's port 8080 and LiveKit's signaling
port 7880 are internal to the stack.

## Checks

- `make lint`: clippy with warnings as errors, plus `rustfmt --check`.
- `make fmt`: format all code.
- `make test`: unit and integration tests.

CI (`.github/workflows/ci.yml`) runs `make lint test` on Linux and the client unit tests on Windows.
