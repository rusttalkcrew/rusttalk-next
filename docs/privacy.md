# Privacy

No telemetry, analytics, crash reporting or tracking.

## What servers store

Written in W06; exact list of plaintext fields.

## What the desktop client stores

The device signing secret, device agreement secret, sync key, person ID and
public device certificate live in Windows Credential Manager or Linux Secret
Service. Development profiles use separate entries. The client logs the
person fingerprint locally. The first launch displays the recovery words in
the terminal. The client never stores or logs the words or the root signing key.

## Outbound connections

- **Bot:** Only the configured RustTalk stack. Signaling and failure-path
  validation use Caddy on 8443/TCP. WebRTC uses the advertised LiveKit node on
  7882/UDP or 7881/TCP. The bot mints its dev token locally.
- **Desktop client, API and index:** No outbound connections in W04. On Linux the
  client accesses Secret Service over the local session bus.
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
