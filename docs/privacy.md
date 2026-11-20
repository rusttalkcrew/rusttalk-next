# Privacy

No telemetry, analytics, crash reporting or tracking.

## What servers store

The API stores its Ed25519 signing seed in `server.key` and the following
tables in SQLite (WAL). IDs and digests are binary unless indicated. The `_ct` suffix denotes encrypted binary data.

| Table | Columns |
| --- | --- |
| `members` | `person_id` (root public key), `joined_at` (Unix seconds), nullable `display_name_ct`, nullable `avatar_ct` |
| `roles` | `id` (text), `kind` (built-in kind or custom), `rank`, `permissions` (seven-bit mask), nullable `name_ct` (opaque ciphertext) |
| `server_settings` | Singleton `id`, nullable `owner_person_id`, `default_role_id`, nullable `name_ct`, nullable `description_ct`, nullable `icon_ct` |
| `member_roles` | `person_id`, `role_id` |
| `invites` | `id` (opaque text), `code_hash` (SHA-256), `is_owner`, `remaining_uses`, `expires_at`, nullable `space_key_ct` and `name_ct` for preview |
| `challenges` | `nonce` (32 random bytes), `ip` (IPv4 address or IPv6 /64), `expires_at` |
| `sessions` | `token_hash` (SHA-256), `person_id`, `device_id` (Ed25519 public key), `ip` (canonical source IP at issuance), `expires_at` |
| `revocations` | `person_id`, `device_id`, `record` (W04 signature, device public key and revocation timestamp) |
| `bans` | `person_id` |
| `ban_ips` | `person_id`, `ip` (only for an explicitly requested IP ban) |
| `audit_log` | `id` (sequence), `actor_person_id`, `action` (fixed event kind), nullable `target_person_id`, nullable `target_id` (opaque object ID), `at` (Unix seconds), nullable `dm_channel_id` (audit visibility) |
| `invite_previews` | `ip` (IPv4 address or IPv6 /64), `expires_at` (60-second rate limit) |
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
| `_sqlx_migrations` | `version`, `description`, `installed_on`, `success`, `checksum`, `execution_time` (migration bookkeeping) |

Caddy supplies the source IP directly. Session IPs are retained with the active
session so a moderator can choose an IP ban. Termination removes the session
row. An explicitly banned IP stays linked to its person ban until unban.
Challenge issuance prunes expired nonces, and sign-in deletes a consumed nonce.
Invite creation prunes expired invites, and redemption deletes spent ones.
Preview requests prune expired rate-limit rows.
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
SQLite backups and old WAL pages can retain past database contents.

The dev TLS certificate and private key live in `.dev/tls/`. Bot data under
`.dev/bots/` holds only saved origins, server IDs and TLS fingerprints.

## What the desktop client stores

The device signing secret, device agreement secret, sync key, person ID and
public device certificate live in Windows Credential Manager or Linux Secret
Service. Development profiles use separate entries. The client logs the
person fingerprint locally. Creating an account shows the recovery words once
in the app window. The client never stores or logs the words or the root
signing key. "Sign out of this device" deletes the keystore entry.

## Outbound connections

- **Bot:** Only the configured RustTalk stack. Membership, key events, encrypted chat, signaling and failure-path
  validation use Caddy on 8443/TCP. WebRTC uses the advertised LiveKit node on
  7882/UDP or 7881/TCP. The bot mints its dev token locally.
- **Desktop client:** Configured RustTalk origins for pinned HTTPS and WebSocket
  connections. On Linux it accesses Secret Service over the local session bus.
- **API and index:** No outbound network connections.
- **Caddy:** Proxies requests to the API on 8080/TCP and to LiveKit on
  7880/TCP, using Docker's internal DNS. Dev TLS uses a local certificate.
- **LiveKit:** Replies to Caddy and to room participants over their WebRTC
  transports. The dev health checks use loopback HTTPS at Caddy and loopback
  HTTP at LiveKit.

Docker image pulls and Cargo dependency downloads happen during setup or build.
The bot's WebRTC build also downloads the pinned LiveKit prebuilt archive from
GitHub Releases.

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
webhooks, Jaeger tracing, Prometheus, TURN and mDNS disabled. The server's
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
