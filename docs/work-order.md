# Work order

Live checklist, copied from section 11 of `PROMPT.md`. Tick an item off with the PR that merges it.

Work strictly in this order. One work item per branch and pull request, named
`w<NN>-<short-name>`. Do not start an item before the previous one is merged and its
gate holds. Keep `docs/work-order.md` as the live checklist: copy this list into it
in W01, then tick items off with the merged PR.

An item is done when `make lint test` is green (plus `make e2e` from W07 on), the
docs it touches are updated, and its gate holds. No item ever stores readable
content on the server, not even temporarily.

**1. Foundation**
- [x] **W01 Repository and Makefile.** Workspace layout (section 3), colorized
  self-documenting Makefile, README, `docs/` outline, `docs/work-order.md`, CI.
  Gate: `make` prints the help; CI runs.
- [x] **W02 Dev stack and logging.** `deploy/compose.dev.yml` with api, livekit, caddy;
  `make dev`, `make logs`, `make down`, `make reset`; client log file and JSON
  service logs with session-close reasons. Gate: `make dev` from a clean checkout
  serves `/v1/health` over TLS by IP address, without a domain.
- [x] **W03 LiveKit by IP, no telemetry.** A bot joins a LiveKit room through caddy
  with a pinned self-signed certificate; all LiveKit outbound reporting is off and
  verified (`docs/privacy.md`). Gate: that bot join works on the dev stack.

**2. Identity**
- [x] **W04 Identity.** 24 words, root key, device keys and certificates, revocation,
  keystore, dev profiles (`make client PROFILE=a`). Gate: unit tests incl. bad
  signatures and revoked devices.
- [x] **W05 Sign-in and membership.** Server key, pinning, challenge sign-in, session
  tokens, owner invite printed once, invites, roles, kick, ban, audit log. Gate: e2e
  invite, join, kick, ban; a changed server key is refused.

**3. Chat (encrypted first)**
- [x] **W06 Keys and zero-knowledge metadata.** Channel and space keys with epochs,
  envelopes, wrapping for new devices and members, full history for newcomers, new
  epoch on kick, ban and revoke; space-key encryption of every name (5.1). Gate: e2e
  kicked bot cannot read the new epoch, new member reads old history, a database
  dump contains none of the test names.
- [x] **W07 Encrypted chat.** Messages, signatures, edits, deletes, 10,000-message
  limit, direct messages, chat UI. Gate: e2e chat both ways; ciphertext only in the
  database.

**4. Voice**
- [x] **W08 Voice.** Rooms, tokens, end-to-end media keys from the channel epoch,
  push-to-talk and voice activation with calibration, audio processing, per-person
  volume, stats panel, non-blocking reconnects. Gate: e2e voice both ways with
  encryption on, and a forced reconnect recovers by itself.

**5. Audio sharing**
- [x] **W09 Per-application audio.** One track per app (Windows and Linux), receiver
  volume per track. Gate: e2e two app tracks with independent volume.

**6. Screen sharing and camera**
- [x] **W10 Screen and window sharing** (Windows and Linux). Gate: e2e screen track.
- [x] **W11 Camera.** Gate: e2e camera track.

**7. File sharing, live**
- [x] **W12 Live transfer** through LiveKit data streams, encrypted, nothing stored,
  2 GiB limit. Gate: e2e transfer between two bots in a room.

**8. File sharing, server-stored**
- [x] **W13 Stored files** with the decided limits and expiry, encrypted thumbnails, no
  file names on the server. Gate: e2e upload and download; limits enforced; volume
  and database hold no plaintext or names.

**9. The rest**
- [x] **W14 Device sync.** Encrypted sync blob on every member server. Gate: two
  profiles of one person sync settings and server list.
- [x] **W15 Moderation polish and multi-server client.** Gate: one client on two dev
  servers at once.
- [ ] **W16 Index server.** Gate: a dev server registers, a client finds it.
- [ ] **W17 Updates and packaging.** Signed releases, verified client updates,
  installer, AppImage, images. Gate: a client updates itself from a local release.
- [ ] **W18 First release.** `make soak` passes; `/ponytail-audit` findings fixed.
