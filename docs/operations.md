# Operations

## Production

`deploy/docker-compose.yml` runs the API, LiveKit and Caddy with restart policies,
named volumes and health checks. The optional directory runs with
`--profile index`. All settings live in `deploy/.env`. Run the commands from
the `deploy/` directory of a checkout of the release you deploy.

1. `cp .env.example .env`, then set `RUSTTALK_IP` to the host's public IPv4
   address and `LIVEKIT_API_SECRET` to the output of `openssl rand -hex 32`.
   Set `RUSTTALK_DOMAIN` to a domain that resolves to that IP for a Let's
   Encrypt certificate. Leave it empty for IP-only mode.
2. Set `RUSTTALK_VERSION` to the release version and
   `RUSTTALK_IMAGE_PREFIX=ghcr.io/rusttalkcrew/`, then `docker compose pull`.
   Without the prefix, Compose builds the images from the checkout.
3. Open 443/TCP for HTTPS, 7881/TCP and 7882/UDP for LiveKit media, and
   80/TCP for Let's Encrypt in domain mode.
4. `docker compose up -d --wait`.
5. Print the owner invite and send it to the owner over a private channel:

   ```sh
   code=$(docker compose exec -T api rusttalk-api owner-invite)
   id=$(docker compose exec -T api rusttalk-api server-id)
   pin=$(docker compose logs --no-log-prefix cert | sed -n 's/^TLS pin: //p' | tail -1)
   echo "rusttalk://<ip>/i/$code#$id.$pin"   # IP-only mode
   echo "rusttalk://<domain>/i/$code#$id"    # domain mode
   ```

   Add `:<port>` after the address if `RUSTTALK_HTTPS_PORT` is not 443.

The owner invite exists only as the output of `owner-invite`. The API logs
the command, never the code. Running the command again replaces the
unclaimed code. After the owner claims the server it prints nothing.

In IP-only mode the `cert` service creates a self-signed certificate for
`RUSTTALK_IP` on first start. The certificate is valid for 100 years and lives
in the `caddy-data` volume. Each start logs its SHA-256 fingerprint and the
base64url pin used in invites (`docker compose logs cert`). The pin changes only
if that volume is lost. In domain mode Caddy keeps its Let's Encrypt account
and certificates in the same volume and renews them itself. Switching modes
needs only a changed `RUSTTALK_DOMAIN` and `docker compose up -d`, but
clients that pinned the self-signed certificate must re-establish trust.

The API and LiveKit share the key ID `rusttalk` and `LIVEKIT_API_SECRET`.
LiveKit's webhook goes to the API on the stack network. `RUSTTALK_DEV` is never
set, so the API refuses the public development secret. Ports 8080 and 7880 stay
on the stack network. `.env.example` lists the optional port and stored file
limit settings.

Back up the `api-data` volume (database, WAL, `server.key`, encrypted files)
and the `caddy-data` volume (TLS certificate and key) as described under
[Ownership and backups](#ownership-and-backups). Stop the API first:

```sh
docker compose stop api
docker run --rm -v rusttalk_api-data:/data:ro -v rusttalk_caddy-data:/caddy:ro \
  -v "$PWD":/backup alpine tar czf /backup/rusttalk-backup.tgz /data /caddy
docker compose start api
```

To upgrade, set `RUSTTALK_VERSION` to the new version in
`.env`, check out the matching release and run `docker compose pull` and
`docker compose up -d --wait`.
The volumes, server key and TLS pin carry over. To roll back, set the previous
version again. Back up first, because a newer API may migrate the database.

## Behind your own reverse proxy

If the host already runs a reverse proxy on 80/443, let it terminate TLS for a
domain and skip the bundled Caddy. Set `RUSTTALK_IP`, `LIVEKIT_API_SECRET`,
`RUSTTALK_VERSION` and `RUSTTALK_IMAGE_PREFIX` in `.env` as above, then:

```sh
docker compose -f docker-compose.yml -f compose.proxy.yml pull
docker compose -f docker-compose.yml -f compose.proxy.yml up -d --wait
```

`compose.proxy.yml` publishes the API on `127.0.0.1:8080`, LiveKit signaling on
`127.0.0.1:7880` and the optional index on `127.0.0.1:8081`
(`RUSTTALK_PROXY_ADDRESS`, `RUSTTALK_API_PORT`, `RUSTTALK_LIVEKIT_PORT`,
`RUSTTALK_INDEX_PORT` change them). LiveKit media still needs 7881/tcp and
7882/udp open to the internet at `RUSTTALK_IP`. A Caddy site:

```caddy
talk.example.com {
	route {
		request_header -X-Forwarded-For
		reverse_proxy /v1/* 127.0.0.1:8080
		reverse_proxy /rtc* 127.0.0.1:7880
		handle_path /index/* {
			reverse_proxy 127.0.0.1:8081
		}
		respond 404
	}
}
```

Any other proxy needs the same three things: route `/v1/` to the API and
`/rtc` to LiveKit with WebSocket upgrades, and set `X-Forwarded-For` to the
client address only (drop what the client sent). The API rate limits and bans
by that address, so do not expose 8080 beyond the proxy. Invites use the
domain without a pin: `rusttalk://talk.example.com/i/<code>#<server-id>`.

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

Set these positive integer environment variables on the API to set the host
ceilings. They are also the defaults until a member with Manage Server saves
limits in **Server settings → Files**. The server clamps saved limits to the
current ceilings when saving and when using them. The dev Compose file passes
the variables through, and `make e2e` applies smaller quota and lifetime values
to its disposable stack.

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
until swept. Lowering a size or quota limit keeps existing files and limits
new uploads. Shorter retention also applies to existing files at the next
cleanup. Files keep their original expiry if retention increases. Run one
API instance per volume. On startup it removes leftover staging blobs and
uncommitted blobs.

## Server emoji limits

| Variable | Default |
| --- | --- |
| `RUSTTALK_EMOJI_MAX` | 100 emoji |
| `RUSTTALK_STICKER_MAX` | 50 stickers |
| `RUSTTALK_EMOJI_MAX_BYTES` | 262,144 (256 KiB per encrypted picture, also the maximum) |

`RUSTTALK_STICKER_MAX` is the host ceiling for the sticker limit in
**Server settings → Files** and the default until a limit is saved. Pictures
live in `emoji/` on the API volume. Lowering a count keeps existing pictures
and stops new uploads of that kind until the count is below the limit. On
startup the API removes pictures without a database row.

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

The API emits one JSON session-close event per closed session. Reasons
include `token_expired`, `kicked`, `banned`, `revoked` and `replaced`.
A token rotation logs `refreshed` while keeping the session open. Failed
requests log static error kinds. A TCP disconnect leaves its bearer token
valid until expiry or invalidation.

The desktop client writes `rusttalk.log` next to its settings in the platform
config directory. It rotates at 5 MiB, keeping one backup (`rusttalk.log.1`),
and creates log files with mode `0600` on Unix. It uses the same level filter
as the API and works on Windows with the console disabled.

### IP bans

IP bans match one IPv4 address or one IPv6 /64, so a person cannot rotate within their own IPv6 prefix to rejoin through open joining. Bans created before W15 still match their exact address. The api only sees real client addresses when Docker forwards the original source IP to Caddy (the default bridge networking with iptables does; rootless Docker or a userland proxy shows every client as the gateway IP, and an IP ban would then hit everyone). Abuse limits: 32 open sign-in challenges per IP (256 in total), 16 sessions and 64 stored revocations per person.

## Automatic updates

The optional updater checks GitHub Releases every five minutes. It verifies
the detached Ed25519 signature over `latest.json`, then accepts only a newer
patch in the running server's configured `<major>.<minor>` series. If GitHub's
latest release is in another series, update the server manually to change series.

### Turn on

From `deploy/`, take a [manual backup](#production) before enabling the updater
or switching from Watchtower. If Watchtower is running, stop and remove the
existing `updater` container first with
`docker compose --profile auto-update stop updater` and
`docker compose --profile auto-update rm -f updater`.

1. Set `RUSTTALK_VERSION=<major>.<minor>` (for example `0.1`) and
   `RUSTTALK_IMAGE_PREFIX=ghcr.io/rusttalkcrew/` in `.env`. Start the server
   manually first if it is stopped or runs a different series.
2. The Compose file supplies the official Ed25519 release public key. If
   the project rotates it, update your Compose file or set the new trusted
   64-character hexadecimal key in `RUSTTALK_RELEASE_PUBLIC_KEY`.
3. Initialize the Compose override once:

   ```sh
   mkdir -p auto-update
   test -f auto-update/images.yml || printf '{"services":{}}\n' > auto-update/images.yml
   ```

   Set `COMPOSE_FILE=docker-compose.yml:auto-update/images.yml` in `.env`.
   Behind your own proxy use
   `COMPOSE_FILE=docker-compose.yml:compose.proxy.yml:auto-update/images.yml`.
   Run Compose from this directory and put deployment settings in `.env` so
   the updater uses the same settings. The last file carries the installed
   image digests. Keep it when running Compose again.
4. Run `docker compose --profile auto-update up -d --no-deps updater`.
   This starts the updater alongside the running stack. On its first check,
   the updater pins the running images before looking for a patch.

The updater mounts the Docker socket, which gives it host-level control. It
can also read your deployment configuration and write the data volumes and
image override. Enable it only if you accept that access.

### What happens during an update

- The updater checks for LiveKit rooms. While a room exists, or LiveKit cannot
  report its status, it holds the update. It installs when rooms clear. After
  `RUSTTALK_UPDATE_HOLD_HOURS` (default 6) it can install during the next
  `RUSTTALK_UPDATE_HOUR_UTC` hour (default 4, meaning 04:00–04:59 UTC) even
  with a call in progress.
- It pulls the signed image digests, rechecks activity, stops the API and
  running index, checkpoints SQLite WALs and archives the full data volumes.
  Expect downtime during the archive and restart. The archive includes the
  server key and stored files.
- It recreates the services by digest and checks their health endpoints and the
  API version. If a candidate fails, it stops it, restores the archive and
  restarts the previous images. Writes accepted by the candidate before rollback
  are reverted. An interrupted transaction is recovered when the updater next
  runs. Let the updater finish before manually recreating either service.
  A failed release stays skipped until a newer patch appears or you install
  it yourself.
- The owner sees pending and last update results in **Server settings →
  Overview**. For details use `docker compose --profile auto-update logs updater`.

### Turn off and restore

Stop and remove `updater` with the commands above. To keep the installed
digests, keep the override in `COMPOSE_FILE`. To manage versions manually,
remove the override entry and move `auto-update/images.yml` out of the
deployment directory. Set `RUSTTALK_VERSION` to an exact release before the
next `docker compose up`.

Backups are UTC-timestamped `rusttalk-*.tar.gz` archives in the project's
`update-backups` volume. The updater keeps the last `RUSTTALK_UPDATE_KEEP`
(default 3). Keep independent copies if you need protection from disk loss.
Each archive includes `release.json` with the previous version and image
references. Treat the archives as private. Allow space for the archive and,
on restore, an unpacked copy of the data.

1. Finish any active update, then stop the updater and the backed-up services.
   Save an independent copy of the current volumes before replacing them.

   ```sh
   docker compose --profile auto-update --profile index stop updater api index
   docker compose --profile auto-update run --rm --no-deps --entrypoint ls updater -l /backups
   ```
2. Replace `ARCHIVE` with the desired filename. Restore replaces the API data
   and, if the archive contains it, the index data.

   ```sh
   docker compose --profile auto-update run --rm --no-deps updater restore ARCHIVE
   ```
3. Remove the updater container and follow the manual version steps above.
   Set the exact version printed by the restore command. Start the stack with
   `docker compose up -d --wait` (include `--profile index` if you ran the
   index). Check `/v1/server-info` and keep the updater off while verifying the
   restored data.

## Releases and updates

A release is one command on an up-to-date `main` whose `Cargo.toml` version
is new:

```sh
make release
```

It signs and pushes the tag `v<version>`. The
[release workflow](../.github/workflows/release.yml) then:

1. builds the Linux AppImage (x86_64, aarch64), the Windows installer
   (x86_64, aarch64) and a universal macOS DMG on native GitHub runners;
2. pushes `ghcr.io/rusttalkcrew/rusttalk-api` and `rusttalk-index` as
   `<version>`, `<major>.<minor>` and `latest` for amd64 and arm64;
3. writes `latest.json` (size and SHA-256 of every installer, and the image
   digests) and signs it with the release key;
4. publishes the GitHub release with all files.

### Release key

One Ed25519 key signs `latest.json`; clients compile its public half in, so
changing it means shipping a new client through a trusted channel. The
maintainer keeps it in `pass` at `apps/rusttalk/release_key`. GitHub holds:

- repository variable `RUSTTALK_RELEASE_PUBLIC_KEY`: the 32-byte public key in hex;
- secret `RUSTTALK_RELEASE_KEY` in the `release` environment, which only `v*`
  tags may use.

```sh
pass show apps/rusttalk/release_key | gh secret set RUSTTALK_RELEASE_KEY --env release
gh variable set RUSTTALK_RELEASE_PUBLIC_KEY --body "$(pass show apps/rusttalk/release_public_key_hex)"
```

`python3 deploy/release/release.py public-key` prints the hex public key of the
key file in `RUSTTALK_RELEASE_KEY`. Verify a release by hand with:

```sh
openssl pkeyutl -verify -pubin -inkey rusttalk-release-public.pem \
  -rawin -in latest.json -sigfile latest.json.sig
```

### Updates

Release builds check
`https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/`
by default; **Settings → Updates** can point at another HTTPS directory
holding `latest.json`, `latest.json.sig` and the installers. **Check now**
checks once. Release builds also check automatically at startup and every
six hours; **Check automatically** turns that off, and a saved choice is kept.
Builds without a default update URL never check on their own. A verified
newer release shows an **Update to vX.Y.Z** button above the user bar (and a
dot on the gear). It opens a confirmation with a **What's new** link (the
GitHub release page for that tag when the directory is a GitHub release
download directory) and **Update** / **Later**. Redirects are followed over
HTTPS only; the signed size and SHA-256 decide what installs.

The Windows installer installs under the user's local application directory,
requires no elevation and restarts RustTalk after an in-app update.
On Linux the AppImage directory must allow the current user to create and
rename files. Linux updates only replace the AppImage that RustTalk was
started from. RustTalk ignores `APPIMAGE` and `APPDIR` values inherited from
other AppImages: it checks that its own AppImage mount carries the file's
name, serves the squashfs image inside that file, and that a running process
was started from that file. Extracted AppImages and
`--appimage-extract-and-run` cannot update themselves. The update replaces
the AppImage atomically and RustTalk restarts from it by itself. Any failure
shows a short error and the current version keeps running.

On macOS one universal DMG serves both `macos-aarch64` and `macos-x86_64` in
`latest.json`. The app is ad-hoc signed only (no Developer ID, not
notarized), so the first start needs right-click → **Open** (see the README).
An update mounts the verified DMG read-only, copies its `RustTalk.app` next
to the running bundle, swaps the two with renames and reopens RustTalk. The
folder holding `RustTalk.app` must allow the current user to create and
rename files; an app that macOS runs from a translocated copy (opened
straight from Downloads) asks to be moved to Applications first. Because
each build has a new ad-hoc signature, macOS may ask again for camera,
microphone, screen recording and keychain access after an update.

The AppImage uses the host's desktop stack, ALSA, PipeWire and libxkbcommon libraries and
is built on Ubuntu 24.04 (glibc 2.39). It packs the
[AppImage runtime](https://github.com/AppImage/type2-runtime) pinned by
SHA-256 in the workflow; `--appimage-license` displays its component notices.

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
