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

Run `make client` in the repository root to build and start the desktop
app. It uses the platform config directory and the OS keystore (Windows
Credential Manager or an unlocked Linux Secret Service session). The window
follows the system light or dark theme.

On first start the app offers two paths:

- **Create account** shows the 24 recovery words once, then asks for 3 of
  them. The device identity is saved to the keystore only after that check
  passes, so closing the app earlier leaves nothing behind.
- **I already have an account** takes the 24 words and adds this computer as
  a new device (`DeviceIdentity::add_device`).

Later starts load the identity from the keystore and open the main window:
server rail, channel list, chat pane and member list. Settings has the
short person fingerprint under Profile > Details, this device's name under
Devices, and "Sign out of this device", which deletes the keystore entry
after a confirmation. Recovery words and keys never reach the log or the
terminal.

To try chat locally, paste the owner invite that `make dev` prints into
"Add a server" in one profile. That profile claims the fresh dev server and
gets the owner menu (click the server name): name the server, create
channels and categories, and make invite links. Paste such a link into a
second profile to join. The join screen shows the server name from the
invite and its fingerprints under "Details". A different server ID or TLS
certificate at a saved address stops the join or reconnect.

Network work runs on a tokio runtime off the UI thread (`crates/client/src/net.rs`).
Each joined server has one task that signs in, keeps the event socket open,
refreshes the token ten minutes before expiry and reconnects with backoff.
The window shows "Connecting…" or "Offline" per server meanwhile. Voice
channels are listed but join in W08.

Run `make client PROFILE=a` for an isolated identity. A profile is exactly
one lowercase letter from `a` to `z`. For profile `a`, the settings and data
directory is `.dev/profiles/a`, which holds `rusttalk.log`, the startup
lock, `servers.json` (pins per address) and `joined.json` (servers to
reconnect at start). The keystore service is `RustTalk-dev-a`. `make reset` removes the
directory and leaves OS keystore entries in place; remove one with
`secret-tool clear service RustTalk-dev-a username identity-v1` on Linux.

`RUSTTALK_APP_ID` sets the Wayland app id and X11 class (default
`rusttalk`). Test runs on a desktop you are using can mark their window so a
compositor rule hides it, for example on Hyprland:

```sh
RUSTTALK_APP_ID=agent.rusttalk make client PROFILE=x
```

A hidden window gets no frame callbacks, so it does not repaint until shown.

Debug builds open a console on Windows; release builds do not, so check
`rusttalk.log` for startup errors there.

## Checks

- `make lint`: clippy with warnings as errors, plus `rustfmt --check`.
- `make fmt`: format all code.
- `make test`: unit and integration tests without Docker.
- `make e2e` starts a disposable stack and runs membership, key distribution,
  and encrypted chat with headless bots. It checks pre-sign-in invite
  previews, historical key delivery, epoch rotation after kick, revocation and loss of
  channel read access, WebSocket closure and redaction in SQLite, its WAL and stack logs. It also
  checks owner bootstrap, invite use, kick, ban, unban, revocation and
  server key pinning. Chat covers both directions in text and DM channels,
  paging, edit authorization, author and moderator deletes, DM isolation and
  closure, historic signatures after kick, and case-insensitive searches for
  test message texts in SQLite, its WAL and service logs. It removes its containers, TLS directory and API
  volume on exit. Run it with ports 8443, 7881 and 7882 free. The chat and membership bot harness builds without the media SDK.

Bots and the desktop client use `rusttalk-core` with the `client` feature.
`client::Server` verifies pins and handles sign-in, and `client::KeyClient`
owns the live stream and keeps keys and accepted message IDs in memory.
The API builds core without the `client` feature. [Protocol reference](protocol.md)

For a new address, use `Server::probe` and show the observed server fingerprint
for confirmation before connecting with that expected pin. Invite links
already specify the expected pins. Use `KeyClient::next_event` while online
to drive key delivery and refresh before `KeyClient::expires_at`.

Use `message_page` for history and `message` for a live record, then call
`open_message` before rendering. Preserve the oldest raw record ID for paging
and retain accepted plaintext in the current UI model. If a key is pending,
reconcile or process key events and retry. Apply edits to their original ID,
hide them when that original is deleted, and reconcile visible history after
`resync` or a reconnect. Recreating a key client starts a fresh replay cache.
Use `refresh` and `reconnect` on the current client to preserve its cache.
For an upgraded W05 stack the owner initializes the first space epoch
after signing in with a W06 device. Existing W05 devices can use the same
recovery words to enroll public W06 certificates. Profiles, server
metadata and any replacement role labels are encrypted on the client
before upload.

Linux CI runs `make lint test e2e`. Lint and unit tests enable core client
protocol and LiveKit pinning code with the bot package excluded. The Docker
gate builds the bot harness without the native WebRTC toolchain or download. Local `make lint test` covers the whole workspace.
Windows CI runs the client unit tests. Both jobs have a 15-minute timeout.
