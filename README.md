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

## Run a server

On a Linux host with Docker, a public IPv4 address and ports 443/tcp,
7881/tcp and 7882/udp open:

```sh
git clone --branch v0.1.0 https://github.com/vleeuwenmenno/RustTalk-cleanroom.git
cd RustTalk-cleanroom/deploy
cp .env.example .env    # set RUSTTALK_IP, LIVEKIT_API_SECRET, optionally RUSTTALK_DOMAIN
docker compose pull && docker compose up -d --wait
```

Already running Caddy, nginx or Traefik on 443? Use
[behind your own reverse proxy](docs/operations.md#behind-your-own-reverse-proxy).
Owner invite, backups and upgrades: [Production](docs/operations.md#production).
Desktop apps: [releases](https://github.com/vleeuwenmenno/RustTalk-cleanroom/releases).

## Docs

- [Architecture](docs/architecture.md)
- [Protocol](docs/protocol.md)
- [Crypto](docs/crypto.md)
- [Privacy](docs/privacy.md)
- [Operations](docs/operations.md)
- [Development](docs/development.md)
- [Work order](docs/work-order.md)

License: all rights reserved.
