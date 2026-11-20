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

## Two development stacks

`make e2e` starts two disposable Compose projects with separate TLS
directories and API volumes. Reserve 8443/TCP, 7881/TCP and 7882/UDP
for the first project and 9443/TCP, 8881/TCP and 8882/UDP for the second.
Signaling uses loopback. Each stack advertises its own LiveKit media
ports at `NODE_IP` (loopback by default under `make e2e`). The target
removes both projects on exit.

For a manual second stack, use another Compose project, a different TLS
directory and the same host and container port for each LiveKit media
transport:

```sh
export RUSTTALK_DEV_USER="$(id -u):$(id -g)"
export RUSTTALK_DEV_DATA="$(mktemp -d)"
export RUSTTALK_DEV_HTTPS_PORT=9443
export RUSTTALK_DEV_MEDIA_TCP_PORT=8881
export RUSTTALK_DEV_MEDIA_UDP_PORT=8882
docker compose -f deploy/compose.dev.yml -p rusttalk-dev-second up --build --wait
```

Use the same project and environment with `down --volumes` to remove it.

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

A LiveKit voice token authorizes room entry for ten minutes after issue.
A data-only transfer token authorizes entry for one minute. Kick, ban and
device revocation end API access, trigger rotation for readers and remove
the device from voice and data rooms. Losing `speak`
revokes microphone and app audio publishing. Losing `video` revokes camera
and screen publishing. Losing `write` revokes data publishing in voice
rooms and removes the affected devices from data rooms. The API
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
in `/data/files/`. Set `RUSTTALK_DATA_DIR` to use another data directory than
`/data`; the index honors it too. Keep this volume and `.dev/tls/` across
normal restarts.
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

IP bans match one IPv4 address or one IPv6 /64, so a person cannot rotate within their own IPv6 prefix to rejoin through open joining. Bans created before W15 still match their exact address. The api only sees real client addresses when Docker forwards the original source IP to Caddy (the default bridge networking with iptables does; rootless Docker or a userland proxy shows every client as the gateway IP, and an IP ban would then hit everyone). Abuse limits: 32 open sign-in challenges per IP (256 in total), 16 sessions and 64 stored revocations per person.

## Releases and updates

Use one Ed25519 release key. Generate the production private key with
`openssl genpkey -algorithm Ed25519 -out /offline/rusttalk-release.pem` on an
offline signing system and keep an offline backup. Keep this file out of
Git, build VMs, Docker contexts and published artifacts. Give the release command its path through `RUSTTALK_RELEASE_KEY` on the
offline release workstation. Cache Cargo and build inputs and import the
Docker base images before attaching the signing key. Derive the public PEM
and hex key with the commands below.

```sh
export RUSTTALK_RELEASE_KEY=/offline/rusttalk-release.pem
openssl pkey -in "$RUSTTALK_RELEASE_KEY" -pubout -out rusttalk-release-public.pem
python3 deploy/release/release.py public-key
```

The last command prints the 32-byte public key as hex. Set
`RUSTTALK_RELEASE_PUBLIC_KEY` to that output on the Windows build VM. Build
both platforms from the same commit and key. The Linux build derives the
public key from the signing key. The clients compile the public key in.
Changing keys requires distributing a new client through a trusted channel.

Build the Windows x64 installer first with
`powershell -ExecutionPolicy Bypass -File deploy/release/windows.ps1 -Iscc <path-to-ISCC.exe>`
from an x64 MSVC developer shell. Use a current-user Inno Setup 6 installation.
Copy the resulting `dist/rusttalk-<version>-windows-x86_64.exe` to `dist/` on
the Linux release workstation. The installer installs under the user's
local application directory, requires no elevation and restarts RustTalk
after an in-app update.

On Linux x86_64, run `make release` with Python 3.11+, OpenSSL 3, Docker
with BuildKit, the client build dependencies and `appimagetool` on PATH.
Use a build host with the oldest glibc you intend to support. The AppImage
uses the host's desktop stack, ALSA and PipeWire libraries. The package
contains the RustTalk client and its PipeWire helper side by side.
`make release` requires `RUSTTALK_APPIMAGE_RUNTIME`: a verified, locally staged x86_64
AppImage runtime, so nothing is downloaded while signing. Keep the source and license notices
for the exact runtime with the publication. `--appimage-license` displays the runtime's
component notices.
[AppImage packager](https://github.com/AppImage/appimagetool/releases)
[AppImage runtime](https://github.com/AppImage/type2-runtime)

The signed release lives at `dist/<version>/`. Its version comes from the
workspace's `Cargo.toml` and uses three numeric components. It contains the
AppImage, the Windows installer, two Docker archives and `latest.json`
with a detached, raw 64-byte Ed25519 signature in `latest.json.sig`.
The manifest lists the base file name, byte count and lowercase SHA-256
for each artifact. Verify the exact manifest bytes with a public PEM obtained from a trusted
source. Compare each artifact's size and SHA-256 to its signed entry.

```sh
openssl pkeyutl -verify -pubin -inkey rusttalk-release-public.pem \
  -rawin -in latest.json -sigfile latest.json.sig
```

Upload the versioned artifacts to a single HTTPS directory on the release
host, then publish `latest.json` and its signature together. Keep the
artifacts listed by any manifest still in use. Enter the directory URL
under **Settings → Profile**. **Check now** checks once. The automatic
option checks once at startup and is off by default. A verified newer
release offers **Install**. On Linux the AppImage directory must allow the
current user to create and rename files.
Linux updates only replace the AppImage that RustTalk was started from.
RustTalk ignores `APPIMAGE` and `APPDIR` values inherited from other
AppImages: it checks that its own AppImage mount carries the file's name,
serves the squashfs image inside that file, and that a running process was
started from that file. Extracted AppImages and `--appimage-extract-and-run`
cannot update themselves.
The update replaces the AppImage atomically and offers **Restart now**.

The release command builds and tags `rusttalk-api:<version>` and
`rusttalk-index:<version>`. To publish to a chosen registry, tag and push
each image. Record its registry digest. The same versioned images can
be distributed as the signed Docker archives and loaded with `docker load`.

```sh
for service in api index; do
  image="rusttalk-$service:$VERSION"
  docker tag "$image" "$REGISTRY/$image"
  docker push "$REGISTRY/$image"
done
```

## Optional index

Build the standalone directory image from the repository root:

```sh
docker build -f services/api/Dockerfile --target index --build-arg SERVICE=index -t rusttalk-index .
```

Run the image with a persistent volume at `/data` (UID/GID 65532).
The index uses `index.sqlite3` in WAL mode and listens on port 8080.
Proxy the port through an HTTPS edge and check `GET /v1/health`.
If the edge strips any incoming `X-Forwarded-For` and supplies exactly one
client address, set `RUSTTALK_INDEX_TRUST_PROXY=1` and keep 8080 accessible
only to that edge. Otherwise the index limits registrations by the direct
TCP peer. Keep request URL and header logging disabled at the edge to
avoid recording searches and credentials. The default lease is three hours.
`RUSTTALK_INDEX_TTL_SECONDS` can shorten it for testing.
`RUSTTALK_INDEX_MAX_LISTINGS` (default 10,000) caps the stored rows, expired
ones included until the minute prune; new server IDs then get `503 index_full`
while listed servers keep refreshing. Registration rate limits track at most
4,096 addresses per minute and refuse new addresses beyond that. Keep system
clocks synchronized so registrations fall within the one-minute window.

`make dev` and `make e2e` enable the Compose `index` profile. To run the dev
Compose stack without a directory, invoke Compose directly and omit the
profile. The dev directory is available to a local bot at
`https://127.0.0.1:8443/index/` with the dev certificate pin. The owner
setting uses `https://caddy:8443/index/` as seen from the API container
and the same TLS fingerprint as the dev server. Use the actual client-facing
HTTPS origin as the advertised server address. See the request fields and
base64url pin format in [the protocol](protocol.md#public-index).

To print the dev certificate pin in the settings format:

```sh
openssl x509 -in .dev/tls/cert.pem -outform DER |
  openssl dgst -sha256 -binary |
  openssl base64 -A | tr '/+' '_-' | tr -d '='
```
