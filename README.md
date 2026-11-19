# RustTalk

Private, self-hosted voice, video and chat for friends and small communities.
Everything people write or name is encrypted on their device; servers only store
and forward ciphertext.

Status: early development. See [docs/work-order.md](docs/work-order.md) for progress.

## Quick start

Requires Docker with Compose and a Rust toolchain. Run `make` to see every
target. Start the stack in one terminal and run the health check in another.
Use Ctrl-C to stop watching for changes before shutting down the stack.

```sh
make dev
curl -fsS --cacert .dev/tls/cert.pem https://127.0.0.1:8443/v1/health
make down
```

## Docs

- [Architecture](docs/architecture.md)
- [Protocol](docs/protocol.md)
- [Crypto](docs/crypto.md)
- [Privacy](docs/privacy.md)
- [Operations](docs/operations.md)
- [Development](docs/development.md)
- [Work order](docs/work-order.md)

License: all rights reserved.
