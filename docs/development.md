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
after a confirmation. Joining a server is not wired up yet: the app checks
a pasted invite link and says so. Recovery words and keys never reach the
log or the terminal.

Run `make client PROFILE=a` for an isolated identity. A profile is exactly
one lowercase letter from `a` to `z`. For profile `a`, the settings and data
directory is `.dev/profiles/a`, which holds `rusttalk.log` and the startup
lock. The keystore service is `RustTalk-dev-a`. `make reset` removes the
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
- `make e2e` starts a disposable stack and runs W05 membership and W06
  key distribution with headless bots. It checks pre-sign-in invite
  previews, historical key delivery, epoch rotation after kick, revocation and loss of
  channel read access, WebSocket closure and redaction in SQLite, its WAL and stack logs. It also
  checks owner bootstrap, invite use, kick, ban, unban, revocation and
  server key pinning. It removes its containers, TLS directory and API
  volume on exit. Run it with ports 8443, 7881 and 7882 free. E2E runs
  locally and stays out of CI for now.

The membership gate uses the bot library's `KeyClient` through the same
pinned TLS origin for HTTPS and live events. It holds keys only in memory.
For an upgraded W05 stack the owner initializes the first space epoch
after signing in with a W06 device. Existing W05 devices can use the same
recovery words to enroll public W06 certificates. Profiles, server
metadata and any replacement role labels are encrypted on the client
before upload.

Linux CI runs `make lint test` with the bot excluded and `rusttalk-core/livekit-tls`
enabled, so it checks pinning and ICE filtering without the native WebRTC
toolchain and download. Local `make lint test` covers the whole workspace.
Windows CI runs the client unit tests. Both jobs have a 15-minute timeout.
