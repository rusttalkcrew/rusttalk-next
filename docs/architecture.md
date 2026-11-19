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

Written in W02.
