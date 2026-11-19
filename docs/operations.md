# Operations

## TLS without a domain

`make dev` generates a self-signed certificate valid for ten years. Caddy
serves it on port 8443. Its names are `127.0.0.1` and `localhost`. The certificate
and private key live in the gitignored `.dev/tls/` directory. Restarts reuse
them, so clients can pin the SHA-256 certificate fingerprint printed by
`make dev`. Resetting the stack replaces that certificate and invalidates
its pin.

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

Once sessions exist, every closed session will log its reason (client left,
token expired, kicked, banned, revoked, or error kind).

The desktop client writes `rusttalk.log` next to its settings in the platform
config directory. It rotates at 5 MiB, keeping one backup (`rusttalk.log.1`),
and creates log files with mode `0600` on Unix. It uses the same level filter
as the API and works on Windows with the console disabled.
