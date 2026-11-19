# RustTalk

Private, self-hosted voice, video and chat for friends and small communities.
Everything people write or name is encrypted on their device; servers only store
and forward ciphertext.

Status: early development. See [docs/work-order.md](docs/work-order.md) for progress.

## Quick start

Requires Docker and a Rust toolchain. Run `make` to see every target.

```sh
make dev      # start the server stack in Docker (available from W02)
make client   # run the desktop client against it (available from W04)
make test     # run the tests
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
