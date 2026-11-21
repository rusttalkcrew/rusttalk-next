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
- `make bot CHANNEL=<voice-channel-id>` starts two bots in an existing
  voice channel and verifies audio both ways. Set `RUSTTALK_BOT_INVITE`
  in the environment to an invite with two available uses. Keep a
  channel reader online to wrap the current key for the new bot devices.
  Ctrl-C stops the bots.
- `make logs` follows all service logs in another terminal.
- `make down` stops the stack and keeps local data.
- `make reset` stops the stack and deletes its volumes and `.dev/` after you
  type `reset`. The next start generates a different TLS certificate and
  server key.

Published ports are 8443/TCP for HTTPS, 7881/TCP for LiveKit media fallback,
and 7882/UDP for LiveKit media. The API's port 8080 and LiveKit's signaling
port 7880 are internal to the stack.

For a single bot, use the same invite environment variable and run it
alongside a peer with the two frequencies reversed.

```sh
cargo run --locked -p rusttalk-bot -- --identity bot-a --channel <voice-channel-id> --tone 440 --expect-tone 660 --once
```

The invite provides the server ID and the TLS pin. A local dev
certificate can supply the TLS pin when the invite omits it. The bot saves
verified pins in `.dev/bots/servers.json` and exits after it hears the
expected tone with `--once`.

On Linux, building the bot or the client requires Clang 21 or later,
`pkg-config`, and GLib and ALSA development headers. The SDK downloads
prebuilt WebRTC during the first build. Building `rusttalk-pipewire`
also needs PipeWire development headers (`libpipewire-0.3-dev` on Debian
or Ubuntu), and libclang for bindgen. The Linux desktop client also needs
libxkbcommon (`libxkbcommon-dev` on Debian or Ubuntu). `make client` and `make bot` put the
helper next to the app and bot. Ship it as a sibling binary in Linux packages.

## Native client

Run `make client` in the repository root to build and start the desktop
app. It uses the platform config directory and the OS keystore (Windows
Credential Manager, the macOS login keychain or an unlocked Linux Secret
Service session). The window follows the system light or dark theme.

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
The window shows "Connecting…" or "Offline" per server meanwhile.

Voice audio runs on its own threads (`crates/client/src/audio.rs`): the
microphone goes through WebRTC's echo cancellation, noise suppression and
automatic gain, then the push-to-talk or voice-activity gate, into the
published track. Each remote track is decoded separately, so every person
has their own volume and mute; the mix plays on the chosen speakers and also
feeds the echo canceller. On Linux the client talks to the sound server over
the PulseAudio protocol (PipeWire serves it too) and falls back to ALSA.
Settings → Voice picks devices, shows the mic level, and calibrates voice
activity ("stay quiet", then "say something"). Push-to-talk works only while
the RustTalk window is focused and no text field has focus. Voice settings
are stored in `voice.json` in the profile folder.

To test audio without the real speakers or microphone, create PipeWire null
sinks and point `voice.json` at them (`"input": "pulseaudio:agent_src.monitor"`,
`"output": "pulseaudio:agent_test"`; Settings hides monitors from the list):

```sh
pactl load-module module-null-sink sink_name=agent_test   # speakers
pactl load-module module-null-sink sink_name=agent_src    # mic: use agent_src.monitor
pw-play --target agent_src some.wav                        # "speak"
pactl unload-module <id>                                   # clean up
```

Run `make client PROFILE=a` for an isolated identity. A profile is exactly
one lowercase letter from `a` to `z`. For profile `a`, the settings and data
directory is `.dev/profiles/a`, which holds `rusttalk.log`, the startup
lock, `servers.json` (pins per address) and `sync.json` (the device sync
snapshot: servers to reconnect at start and synced settings). The keystore service is `RustTalk-dev-a`. `make reset` removes the
directory and leaves OS keystore entries in place; remove one with
`secret-tool clear service RustTalk-dev-a username identity-v1` on Linux.

`RUSTTALK_APP_ID` sets the Wayland app id and X11 class (default
`rusttalk`). Test runs on a desktop you are using can mark their window so a
compositor rule hides it, for example on Hyprland:

```sh
RUSTTALK_APP_ID=agent.rusttalk make client PROFILE=x
```

A hidden window gets no frame callbacks, so it does not repaint until shown.

Off-screen UI tests (for example with `egui_kittest`) set
`files::TEST_ANSWER` so file dialogs return chosen paths instead of opening a
native dialog on the desktop.

Debug builds open a console on Windows; release builds do not, so check
`rusttalk.log` for startup errors there.

### Translations

Every text the app shows comes from `crates/client/locales/<lang>.txt`
(English, Deutsch, Nederlands, 日本語), built into the binary. In code, write
`t!("area.key")`, `t!("area.key", name = value)` for `{name}` placeholders, or
`t!("area.key", count = n)` to pick `area.key.one` / `area.key.other`. Add the
English text to `en.txt` and the same key to every other file; `make test`
fails when code uses a key `en.txt` lacks, a language misses a key, or
placeholders differ between languages.
Never build a sentence from translated pieces: give the whole sentence a key
with placeholders. User content, server and channel names, logs and protocol
strings stay untranslated. Settings → Appearance → Language picks the
language ("System default" follows the computer, else English); the choice is
a synced setting. Japanese glyphs come from a bundled Noto Sans CJK subset
(JIS X 0208, about 1.5 MB).

## Checks

- `make lint`: clippy with warnings as errors, plus `rustfmt --check`.
- `make fmt`: format all code.
- `make test`: unit and integration tests without Docker.
- `make e2e` starts two disposable stacks and runs membership, key distribution,
  encrypted chat, and encrypted voice with headless bots. It checks pre-sign-in invite
  previews, historical key delivery, epoch rotation after kick, revocation and loss of
  channel read access, WebSocket closure and redaction in SQLite, its WAL and stack logs. It also
  checks owner bootstrap, invite use, kick, ban, unban, revocation and
  server key pinning. Chat covers both directions in text and DM channels,
  paging, edit authorization, author and moderator deletes, DM isolation and
  closure, historic signatures after kick, and case-insensitive searches for
  test message texts in SQLite, its WAL and service logs. The same bot exchanges
  encrypted chat on both servers and hears a tone from each in sequence. The
  second stack also exercises open admission, ban listing and audit paging.
  See [Two development stacks](operations.md#two-development-stacks) for ports
  and cleanup. The voice
  scenario checks duplex PCM tones, signed presence notices, a refused
  listen-only publish, recovery after a forced reconnect, and new audio
  after a key rotation that excludes a kicked reader. It also checks that
  taking `speak` from a role stops that speaker's audio, that a moderator's
  voice mute silences the speaker and holds across a rejoin, and that removing
  and moving disconnect them, with the move notice reaching only them. The app-audio check
  uses two `pw-play` processes at 440 and 660 Hz on a disposable
  `rusttalk_e2e_<pid>` null sink created with `pactl load-module module-null-sink`.
  The receiving bot verifies two encrypted names and track sources and both
  frequencies. The test unloads the
  sink and stops both players on exit. It fails when the host has no PipeWire
  or pipewire-pulse session. The screen scenario
  decodes a synthetic 320×180 red-and-blue pattern and its encrypted name,
  verifies an idle viewer receives no video, and checks that a subscribed
  viewer with the wrong key gets no decoded frames and can decode the
  pattern after installing the correct key on the same subscription. The
  camera scenario publishes the same pattern as a camera track,
  decodes it on a second bot, and checks that a member who may speak but
  not share video gets a token LiveKit refuses a camera for. The live file
  scenario revokes data publishing by taking `write`, transfers a 20 MiB
  file to that read-only bot, verifies its SHA-256, watches a third bot's
  raw events for unwanted streams, and searches SQLite, WAL and stack logs
  for the filename and a plaintext marker. The index scenario checks
  owner opt-in, pinned HTTPS search, server ID verification, expiry after
  opt-out with an eight-second test lease, and forged signature rejection.
  Set `E2E_FEATURES=` to run the membership, keys, chat and index harness without WebRTC, voice, app audio,
  screen sharing, camera or live files.
- Run `make soak` before every release. Two headless bots keep their event
  sockets and encrypted voice sessions open for 30 minutes, exchange chat
  every minute, and verify tones in both directions every 30 seconds. The
  disposable stack uses 90-second tokens to exercise refresh on the same
  sockets. Any disconnect, reconnect, failed probe, or API, Caddy, LiveKit,
  or index warning or error fails the run. The target reports and excludes
  Caddy's startup warning for the intentional `admin off` setting and
  LiveKit's data-channel abort and `websocket: close sent` warnings after
  the bots deliberately leave at the end. It prints counts and removes its stack and data.
  Use `SOAK_MINUTES=2 make soak` for a quick check. Override occupied ports
  with `RUSTTALK_DEV_HTTPS_PORT`,
  `RUSTTALK_DEV_MEDIA_TCP_PORT`, and `RUSTTALK_DEV_MEDIA_UDP_PORT`.

For a manual app-audio check, build with
`cargo build --locked -p rusttalk-bot -p rusttalk-pipewire` and run both
bots in the same voice channel with the invite setup above. Bot A uses
`--share-app-audio --expect-tone 660`. Bot B uses
`--tone 660 --expect-app-audio`. Bot A plays its tones for up to 25 seconds.
On Windows app audio requires build 20348 or newer. Test enumeration and
capture in the logged-on user's audio session.

### Screen capture

Build the bot and sibling helper with
`cargo build --locked -p rusttalk-bot -p rusttalk-pipewire`.
Bot A can publish `--share-screen synthetic` and bot B can verify it
with `--expect-screen --once`. Use the same voice channel and invite setup
as for tone bots. Synthetic frames go through the same BGRA conversion
and LiveKit publication as a captured screen.

On Linux, run the helper beside the client or bot. The ScreenCast portal
owns the picker and remote PipeWire connection. Keep the new restore token
locally after each grant and use it only to restore that same selection.
On Hyprland, if the portal only supplies DMA-BUF, add
`screencopy { force_shm = true }` to
`~/.config/hypr/xdph.conf` and restart the Hyprland portal. RustTalk uses
shared-memory BGRx/BGRA buffers.
[Hyprland portal capture source](https://github.com/hyprwm/xdg-desktop-portal-hyprland/blob/master/src/portals/Screencopy.cpp)

For a real headless Linux smoke test, use Sway with a headless-capable
wlroots build, PipeWire, WirePlumber, xdg-desktop-portal,
xdg-desktop-portal-wlr, and a Wayland test client to draw a known pattern.
Use a disposable `XDG_RUNTIME_DIR` and `XDG_CONFIG_HOME` inside
`dbus-run-session`. Set `XDG_CURRENT_DESKTOP=sway`, start PipeWire and
WirePlumber in that runtime directory, and launch Sway with
`WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1` and a minimal config
with a single 1280×720 headless output. Export that compositor's
`WAYLAND_DISPLAY` to the private bus with
`dbus-update-activation-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP`.
Set `[preferred]` and `org.freedesktop.impl.portal.ScreenCast=wlr` in
`$XDG_CONFIG_HOME/xdg-desktop-portal/sway-portals.conf`.
Before starting the portal, put this in
`$XDG_CONFIG_HOME/xdg-desktop-portal-wlr/sway` with the actual output name.

```ini
[screencast]
chooser_type=none
output_name=HEADLESS-1
```

Alternatively, set `chooser_type=simple` with a `chooser_cmd` that prints
`Monitor: HEADLESS-1` on stdout. Start the portals and draw the test
pattern on the private compositor, then publish with
`--share-screen portal` in that session to exercise the real backend.
This mode publishes the portal output under its normal share label. The
automated pattern receiver expects the synthetic label and dimensions.
Stop the private session and its children after the smoke test.
[Portal chooser settings](https://github.com/emersion/xdg-desktop-portal-wlr/blob/master/xdg-desktop-portal-wlr.5.scd)

On Windows, enumerate and capture in the logged-in user's interactive
session. A disconnected RDP session can enumerate sources without
delivering frames. Use an interactive scheduled task for an SSH-driven
smoke test and remove it afterwards.

On macOS, ScreenCaptureKit lists every display and the titled normal windows
of other apps in the current Space; windows in other Spaces or of hidden apps
deliver no frames, so they are left out. The first listing asks for Screen &
System Audio Recording access, and macOS applies a grant only after RustTalk
restarts. Displays capture at their full pixel size and windows at their
size when sharing starts; a resized window is scaled into that size.

### Camera

Cameras use `nokhwa`: V4L2 on Linux, Media Foundation on Windows,
AVFoundation on macOS. On macOS the first listing asks for camera access
and waits for the answer; a refusal fails with a pointer to System Settings.
Its macOS mode list is broken, so `camera.rs` reads modes through
`nokhwa-bindings-macos` and offers only the top rate of each range. MJPEG
frames decode in pure Rust (`image`), other formats through nokhwa, then
convert to I420. Bot A can publish `--share-camera synthetic` (the screen
pattern) or `--share-camera device` (the first camera at 720p30), and bot B
can verify the synthetic pattern with `--expect-camera --once`. In code,
`camera::start` blocks for up to 8 seconds and must run off the UI thread
inside a Tokio runtime context, because LiveKit's video source spawns a task.

Without a webcam on Linux, an existing `v4l2loopback` device can stand in:
feed it with
`ffmpeg -re -f lavfi -i testsrc2=size=1280x720:rate=30 -pix_fmt yuyv422 -f v4l2 /dev/videoN`
and pick it in Settings → Video or with `--share-camera device`. Stop
ffmpeg afterwards.

Bots and the desktop client use `rusttalk-core` with the `client` feature.
`client::Server` verifies pins and handles sign-in, and `client::KeyClient`
owns the live stream and keeps keys and accepted message IDs in memory.
Enable the core `voice` feature for native media. Use
`KeyClient::join_voice`, feed a LiveKit `NativeAudioSource` after
`VoiceRoom::publish_audio` succeeds, and open a `NativeAudioStream` for
each subscribed audio track. Drive `VoiceRoom::next_event` and forward
reconciled key events to `VoiceRoom::sync_key` while in the room. Use
`VoiceRoom::stats` on a timer, and call `VoiceRoom::leave` on user
disconnect or API session closure. The API builds core without the
`client` feature. [Protocol reference](protocol.md)

For a new address, use `Server::probe` and show the observed server fingerprint
for confirmation before connecting with that expected pin. Invite links
already specify the expected pins. Use `KeyClient::next_event` while online
to drive key delivery and refresh before `KeyClient::expires_at`. Use
`KeyClient::next_update` to receive those events alongside presence
changes, and `KeyClient::set_status` to set the member’s status.

Use `KeyClient::set_avatar` and `KeyClient::set_server_icon` to validate
and encrypt images for upload. After decrypting an avatar or icon with
`KeyClient::decrypt_metadata`, use `decode_profile_image` to get RGBA
frames and their delays.

For live files in any readable and writable channel or DM, create
`DataLiveFiles::new(&client, channel_id)` and keep it for the lifetime of local
offers. Use its `offer_live_file` and forward API events to
`handle_live_file_event`. Call `accept_live_file` when the receiver chooses a
new destination. The returned download pumps its own room events and closes
the data connection when it finishes or is dropped.

For in-call transfers, use `VoiceRoom::offer_live_file` to post the offer and
`accept_live_file` with a verified message record and a new destination
path. Call `handle_live_file_event` for API events while polling
`VoiceRoom::next_event` for LiveKit events. The returned `LiveDownload`
emits progress and a terminal event through `next_event`. Cancel it with
`cancel`, or cancel a local offer by transfer ID with `cancel_live_offer`.

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

CI runs `make lint test` on Linux, with core client protocol and LiveKit
pinning code enabled and the bot package excluded, and the client unit tests
on Windows. Both jobs have a 25-minute timeout. Run `make e2e` locally before
every merge; CI does not run it. Local `make lint test` covers the whole
workspace. `rust-toolchain.toml` pins the Rust version, so a new Rust release
does not invalidate the CI caches; update it together with the image in
`services/api/Dockerfile`.

## macOS

The client and core build and test on macOS 14.2 or later with the Xcode
command line tools. `make dev` reads the LAN address from the default
route's interface. `make test` puts Homebrew's `openssl@3` first on `PATH`
for the updater tests, because the system LibreSSL can't make Ed25519 keys
(`brew install openssl@3`). The client and bot build scripts link with
`-ObjC`: WebRTC adds Objective-C category methods that the linker otherwise
drops, and joining a voice channel then aborts with an unrecognized selector.
Cargo doesn't pass on the flag that `webrtc-sys` prints.

Microphone, speakers, camera, screen and window sharing, chat and files
work. App audio and remote control are off and say so, and in-app updates
are unavailable, until their macOS backends land (`docs/work-order.md`, section 10). Started
from a terminal, the client's microphone and camera prompts name the
terminal app, which then holds the permission.

## Local release testing

Run `make release-key` to create `.dev/release-key.pem` and
`.dev/release-public.pem` for local testing. The private PEM is created
with mode 0600. `python3 deploy/release/release.py sign <dir>` writes and
signs `latest.json` for the installers in `<dir>` with the key in
`RUSTTALK_RELEASE_KEY`. Real releases are built and signed by the release
workflow on a tag; see [releases](operations.md#releases-and-updates).

For the headless update gate, build two Linux client versions with
`RUSTTALK_RELEASE_PUBLIC_KEY` set to the output of
`python3 deploy/release/release.py public-key` and
`RUSTTALK_LOCAL_UPDATE_TEST=1` set at compile time. Serve the newer artifact,
its manifest and its signature on `127.0.0.1`. Run the older client with
`RUSTTALK_UPDATE_URL=http://127.0.0.1:<port>/` and `--check-update-now`, then
run the same file with `--version`. Test builds also accept the executable
itself as the install target if run without an AppImage. Use a disposable
copy. Production packages disable these test overrides.
