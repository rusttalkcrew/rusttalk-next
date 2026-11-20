# Operations

## TLS without a domain

`make dev` generates a self-signed certificate valid for ten years. Caddy
serves it on port 8443. Its names are `127.0.0.1` and `localhost`. The certificate
and private key live in the gitignored `.dev/tls/` directory. Restarts reuse
them, so clients can pin the SHA-256 certificate fingerprint printed by
`make dev`. Resetting the stack replaces that certificate and invalidates
its pin.

The bot can pin the same certificate at the host LAN IP. Keep 8443/TCP
reachable for TLS signaling and 7882/UDP plus 7881/TCP reachable at LiveKit's
advertised `NODE_IP` for media. The IP must be reachable even when a bot
uses loopback for signaling. LiveKit's signaling port 7880 stays on the
Compose network. See [LiveKit privacy settings](privacy.md#livekit).

## LiveKit credentials

Set `LIVEKIT_API_KEY` and `LIVEKIT_API_SECRET` in the API service's
environment to match the LiveKit key pair. The development Compose file
uses the public values in `deploy/livekit.dev.yml`. Use a private,
random secret for a real deployment and configure LiveKit's
`webhook.api_key` to match. LiveKit sends signed webhooks to
`http://api:8080/internal/livekit` and the API queries the internal
`http://livekit:7880` RoomService. Keep both endpoints on the stack
network. The API omits credentials and JWTs from logs. Leaving both
variables unset disables voice routes. The API refuses to start with the
public development secret unless `RUSTTALK_DEV=1` is set. Only
`deploy/compose.dev.yml`, which also runs the e2e stack, sets it.

Keep LiveKit's `logging.level` at `warn` or higher to
suppress RoomService access logs that include the API key identifier.
[LiveKit RoomService logging](https://github.com/livekit/livekit/blob/v1.13.11/pkg/service/twirp.go#L101-L145)

A LiveKit token expires ten minutes after issue and authorizes room
entry. Kick, ban and device revocation end API access, trigger rotation
for readers and remove the device from every voice room. Losing `speak`
revokes microphone and app audio publishing. Losing `video` revokes camera
and screen publishing. Losing `write` revokes data publishing. The API
also checks each join, because a token
can outlive the session that requested it. An excluded device that
reaches the relay lacks the new media key, and peers drop its frames
under retired keys. Removal failures appear in API logs as `could not
remove voice access` or `could not check voice access` with a reason.

## Ownership and backups

In the dev project, issue the bootstrap code from an operator terminal:

```sh
docker compose -f deploy/compose.dev.yml -p rusttalk-dev exec api rusttalk-api owner-invite
```

The command writes a single-use code to that terminal. Before the owner
claims it, running the command again replaces the old code. After ownership
exists it emits no code. `make dev` prints the complete bootstrap link if
a new owner can still claim the server. Treat either form as a secret.
The service logs one instruction on first start.

The named `api-data` volume holds `/data/rusttalk.sqlite3` in WAL mode and
`/data/server.key` with mode 0600. It also holds encrypted attachment blobs
in `/data/files/`. Keep this volume and `.dev/tls/` across normal restarts.
Stop the API and back up the whole volume, including any SQLite WAL files
and `files/`, plus the TLS certificate and key. Restrict access to these
backups and restore the database and blob directory from the same backup.
Restore the server key and its mode so returning clients recognize the server.
Replacing either pinned key or certificate requires clients to explicitly
re-establish trust.

Keep port 8080 internal to the Compose network. Caddy strips incoming
`X-Forwarded-For` and sets the single source IP that the API uses for bans.
Keep the API behind this trusted edge.
[Caddy proxy headers](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy#defaults).

## Stored file limits

Set these positive integer environment variables on the API. The dev Compose
file passes them through, and `make e2e` applies smaller quota and lifetime
values to its disposable stack.

| Variable | Default |
| --- | --- |
| `RUSTTALK_FILE_MAX_BYTES` | 33,554,432 (32 MiB per blob) |
| `RUSTTALK_FILE_MEMBER_BYTES` | 524,288,000 (500 MiB per uploader) |
| `RUSTTALK_FILE_SERVER_BYTES` | 5,368,709,120 (5 GiB per server) |
| `RUSTTALK_FILE_TTL_SECS` | 2,592,000 (30 days from successful upload) |

Quotas count committed ciphertext, including tags and thumbnails. At the
default upload limit, 64 KiB chunks leave up to 32 MiB minus 8 KiB for file
plaintext. The current client enforces a 32 MiB ciphertext limit even on a
server configured to accept more. Allow disk space for the database, WAL and
up to four concurrent staging blobs in addition to the quota. The API refuses
downloads at expiry and sweeps eligible blobs and their rows at least every
30 seconds while idle, in batches of 64. Expired blobs count toward quotas
until swept. Changing the lifetime applies to new uploads. Run one API
instance per volume. On startup it removes leftover staging blobs and
uncommitted blobs.

## Logs and health

Check the API through Caddy with:

```sh
curl -fsS --cacert .dev/tls/cert.pem https://127.0.0.1:8443/v1/health
```

`GET /v1/health` returns HTTP 200 and `ok` without touching storage.
Compose uses this HTTPS endpoint to check the dev stack.

`make logs` follows the service logs. The API writes JSON lines to stdout.
Each API request gets a generated ID in its JSON log span and the response
header `X-Request-Id`. The API logs at `info` for RustTalk code and `warn` for
libraries. Headers, URLs and bodies are omitted from API request logs. Caddy
and LiveKit also use JSON logging.

The API emits one JSON session-close event per invalidated token. Reasons
include `token_expired`, `kicked`, `banned`, `revoked`, `refreshed` and
`replaced`. Failed requests log static error kinds. A TCP disconnect
leaves its bearer token valid until expiry or invalidation.

The desktop client writes `rusttalk.log` next to its settings in the platform
config directory. It rotates at 5 MiB, keeping one backup (`rusttalk.log.1`),
and creates log files with mode `0600` on Unix. It uses the same level filter
as the API and works on Windows with the console disabled.

### IP bans

IP bans match one exact address (IPv6 included), so a person with an IPv6 /64 can move to another address. The api only sees real client addresses when Docker forwards the original source IP to Caddy (the default bridge networking with iptables does; rootless Docker or a userland proxy shows every client as the gateway IP, and an IP ban would then hit everyone). Abuse limits: 32 open sign-in challenges per IP (256 in total), 16 sessions and 64 stored revocations per person.
