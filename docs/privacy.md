# Privacy

No telemetry, analytics, crash reporting or tracking.

## What servers store

The API stores its Ed25519 signing seed in `server.key` and the following
tables in SQLite (WAL). IDs and digests are binary unless indicated. The `_ct` suffix denotes encrypted binary data.

| Table | Columns |
| --- | --- |
| `members` | `person_id` (root public key), `joined_at` (Unix seconds), nullable `display_name_ct`, nullable `avatar_ct`, `presence_status` (current choice: online, away or invisible) |
| `roles` | `id` (text), `kind` (built-in kind or custom), `rank`, `permissions` (eight-bit mask), nullable `name_ct` (opaque ciphertext) |
| `server_settings` | Singleton `id`, nullable `owner_person_id`, `default_role_id`, nullable `name_ct`, nullable `description_ct`, nullable `icon_ct`, `open_joining` (public flag), `member_cap` (open admission limit), nullable `index_listing_ct` (encrypted public-listing configuration) |
| `member_roles` | `person_id`, `role_id` |
| `invites` | `id` (opaque text), `code_hash` (SHA-256), `is_owner`, `remaining_uses`, `expires_at`, nullable `space_key_ct` and `name_ct` for preview, nullable `created_by` (person ID of the member who made it) |
| `challenges` | `nonce` (32 random bytes), `ip` (IPv4 address or IPv6 /64), `expires_at` |
| `sessions` | `token_hash` (SHA-256), `person_id`, `device_id` (Ed25519 public key), `ip` (canonical source IP at issuance), `expires_at` |
| `revocations` | `person_id`, `device_id`, `record` (W04 signature, device public key and revocation timestamp) |
| `bans` | `person_id`, `banned_at` (Unix seconds) |
| `ban_ips` | `person_id`, `ip` (IPv4 address or IPv6 /64; only for an explicitly requested IP ban) |
| `audit_log` | `id` (sequence), `actor_person_id`, `action` (fixed event kind), nullable `target_person_id`, nullable `target_id` (opaque object ID), `at` (Unix seconds), nullable `dm_channel_id` (audit visibility) |
| `invite_previews` | `ip` (IPv4 address or IPv6 /64), `expires_at` (60-second rate limit) |
| `open_joins` | `ip` (IPv4 address or IPv6 /64), `expires_at` (one-hour admission limit) |
| `member_devices` | `device_id` (Ed25519 public key), `person_id`, `certificate` (public keys, timestamp, salted name commitment, root signature and format prefix) |
| `categories` | `id` (text), `position`, `name_ct` |
| `channels` | `id` (text), `kind` (text, voice or dm), nullable `category_id`, `position`, nullable `name_ct` and `topic_ct`, nullable `dm_first` and `dm_second` (person IDs) |
| `channel_ids` | `id` (text, including deleted channel IDs to prevent reuse) |
| `channel_permissions` | `channel_id`, `role_id`, `flags` (four-bit mask) |
| `key_scopes` | `id` (text), `kind` (space or channel), nullable `channel_id`, `rotate` |
| `key_epochs` | `scope_id`, `epoch`, `commitment` (HMAC of the epoch context keyed by the epoch key) |
| `key_envelopes` | `scope_id`, `epoch`, `recipient_device`, `sender_device`, `envelope` (HPKE ciphertext, encapsulation and signature) |
| `key_envelope_rejections` | `scope_id`, `epoch`, `recipient_device`, `sender_person` |
| `message_senders` | `device_id`, `person_id`, `certificate` (archived public certificate for signature verification) |
| `messages` | `sequence` (insertion order), `id` and `channel_id` (text), `epoch`, `sender_device`, `sender_person`, `at` (Unix seconds), `size` (binary envelope length), `envelope` (public framing, nonce, ciphertext with tag, and signature) |
| `message_ids` | `id` (opaque text, including deleted message IDs to prevent reuse) |
| `files` | `id` and `channel_id` (opaque text), `uploader` (person public key), `size` (ciphertext bytes), `created_at`, `expires_at` (Unix seconds) |
| `device_sync` | `person_id`, `version`, `ciphertext` (nonce and encrypted payload with tag), `size` (ciphertext bytes), `at` (Unix seconds) |
| `_sqlx_migrations` | `version`, `description`, `installed_on`, `success`, `checksum`, `execution_time` (migration bookkeeping) |

Caddy supplies the source IP directly. Session IPs are retained with the active
session so a moderator can choose an IP ban. Termination removes the session
row. An explicitly banned IP stays linked to its person ban until unban.
Challenge issuance prunes expired nonces, and sign-in deletes a consumed nonce.
Invite creation prunes expired invites, and redemption deletes spent ones.
Preview requests prune expired rate-limit rows. An open admission stores a
source address bucket without a person ID for one hour. A cleanup task
prunes expired admission rows every 30 seconds, and new admissions also
prune them.
The API persists public device certificates. The human-readable device
name and commitment salt stay on the device. Raw invite codes, link keys,
session tokens, space and channel keys, and readable names, descriptions,
topics, icons and avatars are absent from API storage and logs. Audit
records carry IDs and fixed actions. The server knows DM participant IDs,
sender and routing IDs, ciphertext sizes, timestamps and access changes.
Text, replies, attachment references and edit targets are inside message
ciphertext. Explicit deletes and the retention cap remove message rows.
Public sender certificates and audit IDs are retained for history and
accountability.
The API volume stores file blobs, including in-progress uploads and thumbnails,
as ciphertext under opaque disk names. Committed blobs are named only by their
random IDs. File names, types, keys, plaintext lengths and references linking
thumbnails to originals are inside the encrypted message. File operation logs
carry the opaque ID and ciphertext size. The expiry sweep unlinks blobs and
removes their metadata.
Each member has at most one current sync row per server. It is removed with
membership. Settings, joined origins and pins, and contacts are encrypted
on the device before upload. The sync key stays on the person's devices.
SQLite backups and old WAL pages can retain past
database contents, and volume backups can retain expired ciphertext.
The API knows which authenticated member and device have an open event socket,
including when a member chooses to appear offline. It keeps per-person socket
counts in memory and removes them one second after the last socket closes.
The current status choice (online, away or invisible) is stored with the
member row so an invisible member is never announced online when they
reconnect, use another device or the server restarts. Only the latest choice
is kept, and it is returned only to that member. Members see only the person
ID and visible status (online, away or offline). There is no last-seen
timestamp or presence event history. The existing session and connection-close logging
still applies.

Public listing is off by default. The owner explicitly supplies the name
and description they want to make public. The API encrypts this separate
configuration with a key derived from the local server key before writing
SQLite. The publisher opens it in memory for registration, and only the
owner can read it through the settings API. The private space metadata
continues to use the space key.

The index stores the intentionally public name, description, address, server
ID, member count and open-joining flag, plus the registration and expiry
timestamps used to enforce the lease and reject replays. It retains no
members' identities or signatures. It uses source IPs in memory for
registration rate limits, clears them each minute, and does not retain
search terms. Expired entries stop appearing in results immediately and
are later pruned from SQLite.

The API can see who is in which voice room by matching opaque LiveKit
device identities to current channel readers. Readers see unmatched
devices only as a count. After a kick, ban, revocation or permission
change the API asks LiveKit to remove devices that lost access and to
revoke any audio, video or data publication rights they lost. It keeps the active
voice roster in memory for one second per room and drops expired rooms on the
next roster lookup. It keeps no voice roster or
LiveKit join tokens in SQLite. LiveKit sees opaque room and device IDs,
track IDs and source kinds, viewer subscriptions, routing and timing
information, negotiated video codec, dimensions and simulcast layers,
ciphertext sizes, and encrypted media. It can tell that a device shared
a screen, a camera or an app audio track and see its encrypted track
label. Camera tracks carry the fixed label `camera`, never the device
name. Labels
use separate HKDF derivations from the room media key for app audio and
screen video, with a fresh nonce for each publication. They expose the
publication epoch and name length. The chosen monitor or window name,
its pixels, app names, camera pictures and audio are encrypted on the
sender. Capture target IDs, enumerated sources and cameras, and Linux
portal restore tokens stay on the sender's device.

For live files, only the encrypted offer is retained as a chat message.
The API sees its normal message metadata plus the live routing notice's
opaque transfer ID, offering and accepting device IDs, nonce, signature,
timing and whether the receiver accepted or cancelled. It relays that
notice in memory. Outside a call, the API can match short-lived data-room
participants back to authorized devices. LiveKit sees the channel's opaque
room ID, the file topic, opaque stream and transfer IDs, nonce, sender and
accepting device, ciphertext length and timing. The same access changes
that affect voice rooms remove data-room participants who lose read or
write access.
It relays encrypted chunks and keeps no file. The filename, MIME type
and file key are inside the encrypted message. File bytes never pass
through the API. Channel readers can decrypt the offer's metadata and
key, including from history. Only accepting devices receive the stream
from the relay. Receivers keep verified data in a local temporary file
during the transfer, remove it on failure or cancellation, and save the
complete file to the path chosen by the user.

The dev TLS certificate and private key live in `.dev/tls/`. Bot data under
`.dev/bots/` holds only saved origins, server IDs and TLS fingerprints.

## What the desktop client stores

The device signing secret, device agreement secret, sync key, person ID and
public device certificate live in Windows Credential Manager or Linux Secret
Service. Development profiles use separate entries. The client logs the
person fingerprint locally. Creating an account shows the recovery words once
in the app window. The client never stores or logs the words or the root
signing key. "Sign out of this device" deletes the keystore entry and the
profile's `sync.json`, `servers.json` and `saved.json`, so the next person
on that profile starts without the servers, pins or saved file places.
Device settings (`voice.json`, `updates.json`) stay.

Next to the settings, `servers.json` holds each server's address, server ID
and TLS fingerprint. `sync.json` holds the last merged device sync snapshot
with its version: joined servers with their pins, servers left, synced
settings, and local edits not yet uploaded. `voice.json` holds the
chosen audio devices, voice options, the chosen camera's name,
resolution and frame rate, and the server directory address and
certificate pin used by "Find servers" (`index_url`, `index_fingerprint`),
the server IDs of official servers dismissed from the join page
(`dismissed_official`), and the window layout (sidebar widths, the member
list, and how a voice channel splits its call and chat). Of these, only the voice options (push to talk
and its key, activation level, echo cancellation, noise suppression, auto
gain), the camera resolution and frame rate, and the dismissed official
servers sync to the person's other devices; the chosen microphone, speakers and camera stay on this device. Decrypted names, messages and pictures stay in
memory only; the log never contains them, file names or invite links. A
downloaded file is written only where the person picks in the save dialog,
through a temporary file in that folder that is removed if the download fails.
`saved.json` maps each downloaded file's opaque blob ID to where it was saved,
so "Show in folder" and "Open" work later; it never leaves this device.
"Open in default app" in the picture viewer, for a picture not saved yet,
writes it to a `rusttalk-<id>` folder in the system temporary directory and
hands it to the system's default app; the client deletes these folders when it
starts and when the person signs out.
`updates.json` holds the chosen release directory URL and the automatic
check preference on this device.

## Outbound connections

- **Bot:** Only the configured RustTalk stack. Membership, key events, encrypted
  chat, device sync, signaling and failure-path validation use Caddy on 8443/TCP.
  WebRTC uses the advertised LiveKit node on
  7882/UDP or 7881/TCP. The bot requests a voice token from the pinned API.
  The index e2e scenario uses pinned HTTPS to search the dev index on the
  same edge.
- **Desktop client:** Configured RustTalk origins for pinned HTTPS and WebSocket
  connections. Voice uses the same pinned signaling path and advertised
  WebRTC ports as the bot. The client contacts the server directory only
  when the person searches in Find servers: the address set in Settings →
  Privacy, or else the official directory (`rusttalk-index.mvl.sh`, to move
  to `index.rusttalk.com`) with normal certificate trust. The join page lists the
  official servers (`es`, `de` and `jp.public.rusttalk.com`) from a list
  built into the client; it contacts one only when the person picks Join. On Linux it accesses Secret
  Service, and opens file dialogs through xdg-desktop-portal, over the local
  session bus.
- **Update host (opt-in):** The desktop client contacts the HTTPS directory
  configured in Settings when you select **Check now** or opt in to a startup
  check. It requests the release manifest and its detached signature, then
  requests your platform’s artifact if you choose to install. The host sees
  your IP address, requested files and timing. Requests carry no account,
  device or server identifiers. Redirects are disabled.
- **API:** Queries LiveKit RoomService on 7880/TCP using the internal
  Docker hostname `livekit`. When the owner enables public listing and
  saves an index URL, the API posts a signed listing to that HTTPS URL at
  opt-in, on startup and hourly while enabled. The owner's settings prefill
  the official directory, which the owner can change; the API itself has no
  default index URL.
- **Index:** No outbound network connections.
- **Caddy:** Proxies requests to the API on 8080/TCP and to LiveKit on
  7880/TCP, using Docker's internal DNS. With the optional index enabled,
  Caddy also proxies the directory path to the index on 8080/TCP. Dev TLS
  uses a local certificate.
- **LiveKit:** Replies to Caddy and to room participants over their WebRTC
  transports. LiveKit posts signed join, leave and room-end webhooks to
  `http://api:8080/internal/livekit` for presence notifications. This route
  stays behind the Compose network. Caddy exposes the public API and
  signaling paths.
  The dev health checks use loopback HTTPS at Caddy and loopback
  HTTP at LiveKit.

Docker image pulls and Cargo dependency downloads happen during setup or build.
WebRTC builds download the pinned LiveKit prebuilt archive from GitHub
Releases. Linux release builds let appimagetool fetch the AppImage runtime
from GitHub Releases when no local runtime is supplied.

## LiveKit

The dev stack uses the unmodified `livekit/livekit-server:v1.13.11` image
without `--dev`. Compose passes `NODE_IP` as `rtc.node_ip`. The config sets
`rtc.use_external_ip: false` and `rtc.use_ice_lite: true`. An explicit node IP
skips IP discovery. ICE-lite skips server STUN gathering, and the explicit IP
is advertised on `rtc.udp_port: 7882` and `rtc.tcp_port: 7881`.
[LiveKit command flags](https://github.com/livekit/livekit/blob/v1.13.11/cmd/server/main.go#L70-L74),
[pinned RTC config](https://github.com/livekit/mediatransportutil/blob/dd8e31a0adc0/pkg/rtcconfig/config.go#L108-L133),
[WebRTC setup](https://github.com/livekit/mediatransportutil/blob/dd8e31a0adc0/pkg/rtcconfig/webrtc_config.go#L91-L116),
[ICE-lite and STUN defaults](https://github.com/livekit/mediatransportutil/blob/dd8e31a0adc0/pkg/rtcconfig/webrtc_config.go#L223-L235),
[LiveKit ports](https://docs.livekit.io/transport/self-hosting/ports-firewall/).

The open-source server constructs its analytics service without any reporting
clients, so its send methods return immediately. The dev config leaves Redis,
Jaeger tracing, Prometheus, TURN and mDNS disabled. The server's
startup path has no update check.
[Analytics implementation](https://github.com/livekit/livekit/blob/v1.13.11/pkg/telemetry/analyticsservice.go#L66-L110),
[startup](https://github.com/livekit/livekit/blob/v1.13.11/cmd/server/main.go),
[sample config](https://github.com/livekit/livekit/blob/v1.13.11/config-sample.yaml).

With STUN and TURN left empty, LiveKit still advertises public Google and Twilio
STUN servers in signaling. Rust SDK 0.11.0 takes that list when
`RoomOptions.rtc_config.ice_servers` is empty. RustTalk removes the advertised
list from join and reconnect responses in its pinned signaling transport.
The bot then uses host candidates for direct ICE checks against LiveKit.
This covers RustTalk's pinned transport. Other SDK clients can still receive
the server's default list.
[Server ICE list](https://github.com/livekit/livekit/blob/v1.13.11/pkg/service/roommanager.go#L1150-L1157),
[default STUN endpoints](https://github.com/livekit/mediatransportutil/blob/dd8e31a0adc0/pkg/rtcconfig/config.go#L38-L41),
[SDK ICE selection](https://github.com/livekit/rust-sdks/blob/livekit/v0.11.0/livekit/src/rtc_engine/rtc_session.rs#L2880-L2905).

### W03 traffic check

On Linux, after a fresh `make dev`, both `--once` joins reached the WebRTC data
channel with the pinned certificate. One used `127.0.0.1:8443` for signaling,
and one used the host LAN IP on 8443. A wrong pin exited 1 with a certificate
fingerprint mismatch and left no LiveKit participant.

- `tcpdump -nli any -s 128 'ip or ip6'` in the LiveKit container's network
  namespace showed only Caddy traffic on 7880/TCP, the bot's media on
  7882/UDP via Docker's host gateway, and loopback health checks.
- `strace -f -s 0 -yy -e trace=connect,sendto,sendmsg,sendmmsg,write,writev`
  on each bot showed signaling to the requested IP and UDP sends only to
  the LAN node IP on 7882. WebRTC used two temporary UDP sockets for local
  route selection against `8.8.8.8:53` and `[2001:4860:4860::8888]:53`.
  Neither socket sent data. Host `tcpdump` captured zero packets to or from
  either address during the checks.
