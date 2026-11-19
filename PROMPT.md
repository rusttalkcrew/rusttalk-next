# Cleanroom brief: RustTalk

You are building RustTalk from scratch: a private, self-hosted voice, video and chat
app for friends and small communities. Do not look at, copy or port any earlier
RustTalk code. This brief is the only source of truth. When something here is
unclear or contradicts itself, stop and ask; do not guess.

The goal is a small, boring, reliable codebase. Every agent working on it runs in
ponytail mode: invoke `/ponytail` at the start of every session and task (use the
slash command, not the skill tool). Prefer a proven library over our own
implementation, and our own small code over a large framework.

License: closed source for now (all rights reserved; no license file yet). It will
become MIT later, so add no dependency whose license is incompatible with MIT.

## 1. Principles (binding)

1. **Privacy first.** No telemetry, analytics, crash reporting or tracking. No
   outbound connection that is not listed in `docs/privacy.md`. Third-party
   services are opt-in.
2. **Encrypted first, zero-knowledge servers.** Everything a person writes or
   names is encrypted on their device: chat text, attachments, voice, video, shared
   audio, and also server name, description and icon, channel names and topics,
   category names, display names and avatars (see 5.1). Servers store and forward
   ciphertext and know only what they need to route and authorize: opaque IDs,
   person and device public keys, roles, bans, timestamps, sizes, and who is in a
   voice room. Document exactly this list in `docs/privacy.md`.
3. **Lazy code.** The least code that fully works (`/ponytail`). No speculative
   options, layers or features.
4. **Docker first.** Every service runs in Docker, also in local development. Only
   the desktop client runs natively (it needs the GPU, audio devices and screen).
5. **Observable.** Every failure is logged with a reason. Nothing fails silently.
   Logs never contain keys, plaintext, invite codes or recovery words.
6. **Fail closed, but locally.** A bad request or one broken session never takes
   down other sessions or the server.
7. **Simple storage.** Keys live in the OS keystore. No root installers, no custom
   filesystem permission checks. Server data: SQLite (WAL) through `sqlx`, written
   in plain, portable SQL only, so Postgres can be added later if a real server
   ever measures the need; encrypted files live on disk, not in the database.

## 2. Architecture

```
            ┌──────────── self-hosted server (docker compose) ────────────┐
 client ──► │ caddy (TLS edge) ─► api   (Rust, axum, SQLite, blob volume)  │
  (Rust,    │                 └► livekit (upstream image, unmodified)      │
   egui,    └───────────────────────────────────────────────────────────────┘
  livekit
   SDK)  ──► index (optional, separate deployment: public server directory)
```

- **caddy:** TLS termination and routing. `/v1/*` goes to `api`, `/rtc*` goes to
  LiveKit signaling. With a public domain it uses Let's Encrypt; without one it uses
  a self-signed certificate that clients pin (see 4.3). LiveKit media (UDP, plus a
  TCP fallback port) goes straight to the `livekit` container.
- **api:** our only server code. Identity checks, membership, roles, channels,
  ciphertext message storage, encrypted file blobs, encrypted sync blobs, LiveKit
  token minting, one WebSocket for live events. SQLite (WAL) plus a file volume.
- **livekit:** the media relay (SFU). It knows nothing about identities; it only
  accepts short-lived tokens minted by `api`. Configure it with telemetry and any
  outbound reporting disabled, and verify that in its source and docs before
  adopting it (record the result in `docs/privacy.md`).
- **index:** a separate, optional service where servers may list themselves.
- **client:** native desktop app (Windows, Linux, Steam Deck).
- **bot:** a headless client built on `crates/core` and the LiveKit SDK, used by the
  end-to-end tests (joins, chats, sends synthetic audio and video).

Language: Rust everywhere except the unmodified LiveKit and Caddy images.
Client UI: `egui`/`eframe`. Media: the official LiveKit Rust SDK.

## 3. Repository and developer workflow

```
crates/core      identity, crypto, protocol types (shared by client, bot, api)
crates/client    desktop app
crates/bot       headless test client
services/api     server
services/index   directory service
deploy/          docker-compose.yml (production), compose.dev.yml, caddy, livekit config
docs/            architecture, protocol, crypto, privacy, operations, development
Makefile         colorized, self-documenting
README.md        what it is, quick start (3 commands), links into docs/
```

Make targets (all colorized; `make` alone prints the help with groups):

| Target | Does |
|---|---|
| `make dev` | builds and starts the whole server stack in Docker (api, livekit, caddy, index) with live rebuild of `api` on change; prints the address, server fingerprint and a fresh owner invite |
| `make client` | runs the desktop client natively against the dev stack |
| `make client PROFILE=a` | same, with an isolated profile (`a`–`z` only): own settings, own keystore namespace, own data folder under `.dev/profiles/<p>` |
| `make bot` | starts headless bots that join the dev server and chat/talk |
| `make test` | unit and integration tests (Docker for the stack) |
| `make e2e` | stack plus 2 bots: E2E chat, voice, camera, screen, per-app audio, files |
| `make soak` | stack plus 2 bots for 30 minutes idle plus voice; fails on any disconnect or error log |
| `make lint` / `make fmt` | clippy (deny warnings) and rustfmt |
| `make logs` | follow all service logs |
| `make down` / `make reset` | stop; reset deletes dev data after typing `reset` |
| `make release` | release builds and images |

Dev data lives in `.dev/` (gitignored). The dev stack uses a fixed local address and
prints everything a developer needs; no manual steps between `make dev` and a
working `make client`.

## 4. Identity

### 4.1 Person and devices
- On first start the client creates 24 BIP-39 words. They are shown once, then
  confirmed by typing 3 random words.
- From the words: `seed` → HKDF → **root key** (Ed25519) and **sync key**
  (32 bytes). The person ID is the root public key. A short fingerprint of it is shown
  everywhere a person is identified.
- Each device creates its own **device keys**: Ed25519 (signing) and X25519 (key
  agreement). The root key signs a **device certificate**: device public keys,
  device name, creation time.
- The root key is used only to sign device certificates and revocations, then
  forgotten. The client never stores the words or the root key. Admin actions on
  the identity (add or revoke a device) ask for the words again.
- **Add a device:** type the 24 words on the new device. It creates its own device
  keys and certificate. Same flow is recovery.
- **Revoke a device:** a root-signed revocation (device public key, time). Any
  device uploads it to every server the person is a member of; servers drop that
  device's sessions and stop accepting it.
- Device keys and the sync key are stored in the OS keystore (Windows Credential
  Manager, Linux Secret Service). Dev profiles use a keystore namespace per profile.
- Display names are per server and chosen by the person; they are not part of the
  identity.

### 4.2 Signing in to a server
1. `GET /v1/auth/challenge` returns a random challenge.
2. The client sends its device certificate and a signature over
   `challenge || server_id || "rusttalk-auth-v1"` with its device key.
3. `api` verifies the certificate against the person ID, checks the device is not
   revoked and the person is a member (or is redeeming an invite), and returns a
   session token (random, 12 hours, bound to that device). Tokens are refreshed
   silently while connected.

### 4.3 Server identity (must work without a domain)
- Each server has an Ed25519 **server key**; `server_id` is its public key.
- Invites carry the server address, the server ID and, in self-signed mode, the TLS
  certificate fingerprint. The client pins both on first use. A different server ID
  or certificate at a known address is a hard stop with a clear message.
- Joining by address without an invite shows the fingerprint for the person to
  compare, then pins it.
- A server reachable only by IP address (no domain, self-signed certificate) is a
  first-class setup, not a fallback. Before step 2, verify that the LiveKit Rust
  SDK can connect to a pinned self-signed certificate; if it cannot, find the
  smallest fix (custom TLS connector, or routing signaling through a client-side
  pinned connection) and document it.

## 5. Servers, membership, roles

- Roles: Owner, Admin, Moderator, Member. One owner; the owner can transfer it.
- On first start the server prints an owner invite once, to its console only (never
  to logs).
- Invites: code or link, limited uses and expiry, created by Admins (and Moderators
  if allowed). Optional open joining with rate limits (per IP and per hour) and a
  member cap.
- Kick, ban (person, optionally IP), unban, role changes, audit log (who did what,
  when; no message content).
- Channels: text and voice, in categories. Per channel: who can read, write, speak,
  share video. Keep the permission model to these four flags per role.
- A client can be connected to several servers at the same time.

### 5.1 Zero-knowledge server metadata
- Each server has a **space key** with epochs, delivered exactly like channel keys
  (6.1) to every member device.
- Encrypted with the space key: server name, description and icon; channel,
  category and role names; channel topics; member display names and avatars.
- In plaintext on the server, only what it needs: IDs, channel kinds and order,
  the four permission flags per role, role assignments, bans, invites (stored as
  hashes), the audit log (actions and IDs only).
- An invite carries the current space key encrypted under a key in the invite
  link, so a new member can show the server's name before anyone online wraps keys
  for them.
- Public listing in the index (section 8) is the owner's explicit choice to
  publish name and description in plaintext there.

## 6. End-to-end encrypted chat (build first)

### 6.1 Channel keys
- Each text channel (and each voice channel, for its media) has a **channel key**
  with an epoch number. The first key is created by the channel's creator.
- The key is delivered as **envelopes**: the key encrypted (HPKE, X25519) to each
  member device's X25519 key. `api` stores envelopes and hands each device its own.
- When a member or device joins, any online member client that holds the key
  verifies the newcomer's device certificate and uploads envelopes for it. The
  newcomer always receives all earlier epochs, so new members read the full
  history (up to the 10,000-message limit).
- When a member is kicked or banned, or a device revoked: the next online member
  client creates a new epoch and wraps it for the remaining devices only.
- Direct messages are a channel with exactly two persons; same mechanism.
- Document the trust model: the server decides membership, so a malicious server
  could add a fake member. Clients show every member and device change in the
  channel, with fingerprints.

### 6.2 Messages
- Message plaintext: text (max 2,000 characters), optional reply-to, attachment
  references, edit-of reference.
- Encrypted with XChaCha20-Poly1305 under the channel key of the current epoch.
  Associated data: channel ID, epoch, message ID, sender device.
- Signed by the sender's device key, so the server cannot forge or alter messages.
- `api` stores ciphertext, sender device, timestamp and size. At most 10,000
  messages per channel; the oldest are dropped.
- Edits are new signed messages that reference the original. Deletes: the author
  deletes their own; Moderators delete any (server-side removal, logged in the
  audit log without content).
- Typing indicators, read state and reactions are not in the first version.

### 6.3 Files and images
Two ways, both encrypted on the sender's device with a random per-file key (the key
travels inside the encrypted message):

1. **Server-stored:** the ciphertext is uploaded to `api` and kept on its volume.
   Limits (server config, these defaults are decided): 32 MiB per file, 500 MiB per
   member, 5 GiB per server, files expire after 30 days. File names and types are
   inside the encrypted message, never sent to the server. Images get an encrypted
   thumbnail made by the sender.
2. **Live transfer:** sent through LiveKit's data streams to members who are online
   in the same channel; nothing is stored. Limit 2 GiB per file. (This is relayed by
   the media server, not peer-to-peer; it stays end-to-end encrypted.)

Received media is treated as hostile: decode images with a memory-safe decoder and
size limits.

### 6.4 Encrypted sync between your devices
- Settings, server list (with pins) and contacts form one blob, encrypted with the
  sync key, max 256 KiB, with a version number.
- Stored on every server the person is a member of (`PUT/GET /v1/me/sync`); the
  newest version wins. Devices merge on start and after changes.

## 7. Voice, video and sharing (LiveKit, full coverage)

- A voice channel is a LiveKit room. `api` mints a token (10 minutes, room-scoped,
  with publish rights from the channel permissions) when a member joins.
- **End-to-end media encryption:** LiveKit's frame encryption with a shared key per
  room, derived (HKDF) from the voice channel's current channel-key epoch. A new
  epoch rotates the media key.
- **Microphone:** push-to-talk or voice activation with a calibration step and a
  visible input meter. Echo cancellation, noise suppression and auto gain from the
  SDK's audio processing. An optional stronger AI noise filter can be installed
  later; leave a clean hook for it.
- **Camera:** pick device, resolution and frame rate; simulcast on.
- **Screen and window sharing:** Windows Graphics Capture on Windows;
  xdg-desktop-portal ScreenCast plus PipeWire on Linux. Pick screen or window,
  resolution and frame rate, optional cursor.
- **Per-application audio sharing:** each chosen application is captured separately
  (Windows: process loopback capture; Linux: PipeWire per-node capture) and
  published as its own audio track, named after the app. Listeners see each shared
  app as a separate stream and set its volume or mute it locally.
- **Listener controls:** per-person and per-stream volume, mute, deafen; choose
  which videos to watch (only subscribed tracks are received).
- **Stats panel:** ping, jitter, packet loss in and out, bitrate per track, taken
  from the SDK's WebRTC stats (never counted by hand).
- Leaving, device changes and reconnects never block the UI; show a "connecting"
  state per person instead.

## 8. Index server

- Servers may register: name, description, address, server ID, member count,
  open-joining flag, signed with the server key. Re-register every hour; entries
  expire after 3 hours.
- Clients can browse and search it. Listing is opt-in for server owners; the client
  only contacts the index when the person opens the browser.

## 9. Updates, packaging, logging

- Releases: Windows installer (per user, no admin), Linux AppImage, Docker images
  for `api` and `index`. Release artifacts are signed; the client verifies the
  signature and SHA-256 before installing an update.
- Client log: `rusttalk.log` next to the settings, owner-only, 5 MiB rotation, also
  on Windows builds without a console. Level `info` for our code, `warn` for
  libraries.
- Service logs: JSON lines to stdout with a request ID; every closed session logs
  why (client left, token expired, kicked, banned, revoked, error kind).
- `GET /v1/health` for Docker health checks (no secrets, no database writes).

## 10. Testing (enough, not more)

Test what protects people, nothing else:
- **Unit tests** only for crypto, key envelopes, signatures, protocol parsing and
  limits at trust boundaries, each with its bad paths (wrong signature, wrong epoch,
  truncated, replayed, oversized).
- **One end-to-end scenario per feature** with bots against the real Docker stack:
  the happy path plus the one security property that matters (for chat: the
  database holds only ciphertext; for kick: the kicked bot cannot read the new
  epoch).
- **No tests** for UI layout, getters, glue code, logging format or anything a
  reader verifies at a glance. Do not mock what a bot can exercise for real.
- `make soak` (30 minutes, idle plus voice, two bots, zero disconnects and zero
  error lines) runs before every release, not on every change.
- At the end of every phase, run `/ponytail-audit` on the repository and fix what
  it ranks highest: dead code, needless layers, risky code without a test, and
  tests that test nothing.
- CI runs `make lint test e2e` on Linux and the client unit tests on Windows.

## 11. Work order

Work strictly in this order. One work item per branch and pull request, named
`w<NN>-<short-name>`. Do not start an item before the previous one is merged and its
gate holds. Keep `docs/work-order.md` as the live checklist: copy this list into it
in W01, then tick items off with the merged PR.

An item is done when `make lint test` is green (plus `make e2e` from W07 on), the
docs it touches are updated, and its gate holds. No item ever stores readable
content on the server, not even temporarily.

**1. Foundation**
- **W01 Repository and Makefile.** Workspace layout (section 3), colorized
  self-documenting Makefile, README, `docs/` outline, `docs/work-order.md`, CI.
  Gate: `make` prints the help; CI runs.
- **W02 Dev stack and logging.** `deploy/compose.dev.yml` with api, livekit, caddy;
  `make dev`, `make logs`, `make down`, `make reset`; client log file and JSON
  service logs with session-close reasons. Gate: `make dev` from a clean checkout
  serves `/v1/health` over TLS by IP address, without a domain.
- **W03 LiveKit by IP, no telemetry.** A bot joins a LiveKit room through caddy
  with a pinned self-signed certificate; all LiveKit outbound reporting is off and
  verified (`docs/privacy.md`). Gate: that bot join works on the dev stack.

**2. Identity**
- **W04 Identity.** 24 words, root key, device keys and certificates, revocation,
  keystore, dev profiles (`make client PROFILE=a`). Gate: unit tests incl. bad
  signatures and revoked devices.
- **W05 Sign-in and membership.** Server key, pinning, challenge sign-in, session
  tokens, owner invite printed once, invites, roles, kick, ban, audit log. Gate: e2e
  invite, join, kick, ban; a changed server key is refused.

**3. Chat (encrypted first)**
- **W06 Keys and zero-knowledge metadata.** Channel and space keys with epochs,
  envelopes, wrapping for new devices and members, full history for newcomers, new
  epoch on kick, ban and revoke; space-key encryption of every name (5.1). Gate: e2e
  kicked bot cannot read the new epoch, new member reads old history, a database
  dump contains none of the test names.
- **W07 Encrypted chat.** Messages, signatures, edits, deletes, 10,000-message
  limit, direct messages, chat UI. Gate: e2e chat both ways; ciphertext only in the
  database.

**4. Voice**
- **W08 Voice.** Rooms, tokens, end-to-end media keys from the channel epoch,
  push-to-talk and voice activation with calibration, audio processing, per-person
  volume, stats panel, non-blocking reconnects. Gate: e2e voice both ways with
  encryption on, and a forced reconnect recovers by itself.

**5. Audio sharing**
- **W09 Per-application audio.** One track per app (Windows and Linux), receiver
  volume per track. Gate: e2e two app tracks with independent volume.

**6. Screen sharing and camera**
- **W10 Screen and window sharing** (Windows and Linux). Gate: e2e screen track.
- **W11 Camera.** Gate: e2e camera track.

**7. File sharing, live**
- **W12 Live transfer** through LiveKit data streams, encrypted, nothing stored,
  2 GiB limit. Gate: e2e transfer between two bots in a room.

**8. File sharing, server-stored**
- **W13 Stored files** with the decided limits and expiry, encrypted thumbnails, no
  file names on the server. Gate: e2e upload and download; limits enforced; volume
  and database hold no plaintext or names.

**9. The rest**
- **W14 Device sync.** Encrypted sync blob on every member server. Gate: two
  profiles of one person sync settings and server list.
- **W15 Moderation polish and multi-server client.** Gate: one client on two dev
  servers at once.
- **W16 Index server.** Gate: a dev server registers, a client finds it.
- **W17 Updates and packaging.** Signed releases, verified client updates,
  installer, AppImage, images. Gate: a client updates itself from a local release.
- **W18 First release.** `make soak` passes; `/ponytail-audit` findings fixed.

## 11a. Agents and models

Quality over cost. Every agent runs `/ponytail` first and works at high effort
(`xhigh` for crypto, identity, keys, permissions and anything security related).

- **Implementation (default):** Codex (ChatGPT) on the Work account; when its
  usage runs out, the Personal account; when both run out, Claude Opus.
- **Anything GUI** (client UI, layout, screens, interaction): always Claude Opus.
- **Light work** (scouting, reading logs, docs wording, small mechanical edits):
  Grok 4.7.
- **Reviews:** a different model family than the author, read-only, before each
  merge of a security-related item and at the end of every phase.

## 12. Decisions already made

1. Servers without a domain (IP address, self-signed certificate) must work.
2. New members can read all older messages of the channels they may read.
3. File limits: 32 MiB per file, 500 MiB per member, 5 GiB per server, 30 days.
4. No GIF search, and no other third-party content service, for now.
5. Closed source for now; MIT later.
6. Every agent works with `/ponytail`.
