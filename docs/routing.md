# Per-app network routing (SOCKS5, WireGuard, Tor)

RustTalk can route **only its own network traffic** directly (default) or through **SOCKS5**, an app-only **WireGuard** tunnel, or **Tor**, without changing system-wide routing tables, VPN profiles, or any other app's traffic.

You can pick a route on the first onboarding screen—before RustTalk makes its first network request—and change or test it later in **Settings → Network** on Linux, macOS, Windows, Android, and iOS.

## How each mode works

| Feature | Direct (default) | SOCKS5 (`socks5h`) | WireGuard (app-only userspace) | Tor (`socks5h`) |
| --- | --- | --- | --- | --- |
| Chat, DMs, forums, calendar, sync, stored files (up to 32 MiB) | Direct HTTPS + WSS | Routed via SOCKS5h | Routed via encrypted WireGuard tunnel | Routed via Tor SOCKS5h |
| Server discovery, invites, update checks/downloads, AI noise model, link previews, pasted web pictures | Direct HTTPS | Routed via SOCKS5h | Routed via encrypted WireGuard tunnel | Routed via Tor SOCKS5h |
| DNS for destination hosts | System DNS | Remote DNS on the proxy (`ATYP=0x03`, zero local DNS) | In-tunnel UDP DNS to configured WireGuard `DNS` servers (zero host DNS) | Remote DNS over Tor (`ATYP=0x03`, supports `.onion`, zero local DNS) |
| End-to-end encryption & server certificate pinning | Enforced | Enforced | Enforced | Enforced |
| Voice/video calls, screen share, shared terminal, remote control, LiveKit live file transfers | Available | Blocked (fail closed) | Blocked (fail closed) | Blocked (fail closed) |
| Desktop SIP phone calls | Available | Blocked (fail closed) | Blocked (fail closed) | Blocked (fail closed) |
| Changes to host routing or other apps | None | None | None | None |

### Why calls, live file transfers, and SIP are blocked when app-only routing is active

- **LiveKit / WebRTC (`libwebrtc`):** The upstream `livekit` / `libwebrtc` C++ stack opens direct OS UDP/TCP sockets and gathers local host ICE candidates without exposing a Rust proxy or custom socket-factory hook (`webrtc-sys`). Allowing `Room::connect` under app-only SOCKS5, userspace WireGuard, or Tor would silently bypass your chosen route and disclose local IP addresses via ICE candidates. RustTalk therefore blocks calls and LiveKit-based live file transfers with a clear explanation instead of leaking direct traffic.
- **Tor:** Tor carries TCP streams only and cannot transport real-time WebRTC UDP media or SIP/RTP audio.
- **Desktop SIP phone calls:** `rsipstack` and RTP audio use direct OS UDP sockets and system DNS, so RustTalk disables them whenever a non-Direct route is selected.
- **Server-stored file transfers** (up to 32 MiB) work normally over HTTPS through SOCKS5, WireGuard, and Tor.
- **If you need calls over WireGuard:** Use your operating system's WireGuard VPN client (or an OS per-app VPN) with RustTalk set to **Direct**. App-only OS VPN routing (`VpnService` on Android, `NEPacketTunnelProvider` Network Extension on iOS/macOS, TUN/Wintun on desktop) requires platform VPN entitlements or elevated privileges that RustTalk intentionally does not request.

## Setup

### SOCKS5

1. Open **Settings → Network** (or choose **SOCKS5** on first start).
2. Enter the proxy **Host** (IPv4, IPv6, or hostname) and **Port** (default `1080`).
3. Optionally enter a **Username** and **Password** (RFC 1929).
4. Select **Test route**, then **Apply & reconnect**.

RustTalk always uses `socks5h` (remote hostname resolution) and ignores OS/environment proxy variables. The only direct connection is to the SOCKS5 proxy endpoint itself (and DNS for the proxy hostname if you did not enter an IP literal).

**Sender-side link preview boundary under SOCKS5 and Tor:** When optional sender-side link previews are enabled under SOCKS5 or Tor, RustTalk blocks literal private/loopback IP addresses and local hostnames (`.localhost`, `.local`, `.internal`, single-label names) locally without calling OS DNS, then delegates hostname resolution to the remote proxy (`ATYP=0x03`). Because the client does not see what IP the remote proxy resolves a public-looking hostname to, blocking connections to private networks reachable from the proxy itself depends on the proxy/Tor exit policy. Under app-only WireGuard, RustTalk resolves the hostname once inside the tunnel, verifies every returned IP is public, and pins that exact IP over `socks5://` with a strict resolver override so a second DNS lookup (DNS rebinding) cannot occur.

### WireGuard (app-only userspace)

RustTalk runs WireGuard completely inside the app process (`boringtun` Noise_IKpsk2 + `smoltcp` dual-stack IPv4/IPv6 TCP & DNS) over a single unprivileged UDP socket—no root/administrator access, no kernel TUN interface, and no effect on other apps.

1. Choose **WireGuard** and expand **Paste WireGuard .conf** to import a standard `[Interface]` / `[Peer]` configuration file, or fill in the fields manually:
   - **Peer endpoint (`host:port`):** IPv4 (`203.0.113.10:51820`), IPv6 (`[2001:db8::1]:51820`), or hostname. Using an IP literal avoids even a single direct DNS lookup for the endpoint.
   - **Peer public key** and **Private key** (standard 32-byte base64 WireGuard keys), plus optional **Preshared key**.
   - **Tunnel addresses (CIDR):** Your assigned IPv4 and/or IPv6 tunnel address(es), e.g. `10.66.0.2/32, fd42::2/128`.
   - **Tunnel DNS servers:** IPv4 and/or IPv6 DNS server IP(s) reachable inside the tunnel (e.g. `10.66.0.1` or `1.1.1.1`). Required to connect to hostnames; if left empty, only IP literals can connect (RustTalk never falls back to host OS DNS).
   - **Allowed IPs (CIDR):** Defaults to `0.0.0.0/0, ::/0`. Any destination IP outside `AllowedIPs` is refused—never sent direct.
   - **MTU** (default `1280`, range `576–1500`) and **Persistent keepalive** (default `25` seconds).
2. Select **Test route** to verify the Noise handshake with the peer, then **Apply & reconnect**.

### Tor

RustTalk connects to a local or user-configured Tor SOCKS5 proxy (`socks5h`) so you can use your system Tor daemon, bridges/pluggable transports (obfs4, Snowflake), or `.onion` RustTalk servers:

- **Linux / macOS / Windows:** Run `tor` (default `127.0.0.1:9050`) or Tor Browser (`127.0.0.1:9150`).
- **Android / iOS:** Run **Orbot** (SOCKS5 on `127.0.0.1:9050`).
- Optionally set **Stream isolation username / password** to isolate RustTalk's Tor circuits from other clients sharing the same Tor daemon.
- Note: RustTalk does not embed an in-process Tor router (such as Arti); you must bootstrap Tor/Orbot before activating the Tor route.

## Failure behavior (fail closed, no silent fallback)

- **Onboarding:** On a fresh profile, all background tasks—including automatic update checks—wait until you confirm a route.
- **Cold start:** If `route.json` specifies SOCKS5, WireGuard, or Tor, RustTalk blocks outbound app traffic while connecting. If the proxy is unreachable, the WireGuard handshake fails, `route.json` is unreadable, or required secrets are missing from secure storage, RustTalk enters the **Blocked** state and displays why.
- **Route switch:** Applying a new route closes active server connections, event WebSockets, calls, live file transfers, link preview workers, and cached update clients, then reconnects through the new route.
- **No automatic fallback:** RustTalk never silently falls back to **Direct**. Returning to **Direct** always requires an explicit click (**Switch to Direct** or selecting **Direct** and applying).

## Credential storage

- Non-secret route settings live per device in `route.json` and are **never** included in encrypted device sync (`sync.json`).
- Secrets (SOCKS5 password, Tor password, WireGuard private key, WireGuard preshared key) are never written to `route.json` or logs:
  - **Linux, macOS, Windows:** OS keychain (Secret Service, macOS Keychain, Windows Credential Manager). On desktops without a keychain (such as Steam Deck Game Mode), stored in a user-only file (`route.secret`, mode `0600` on Unix) with a warning in Settings.
  - **Android:** Wrapped with a non-exportable Android Keystore AES-256-GCM key (`route.wrapped`, excluded from backups). Never written in plaintext.
  - **iOS:** Data-protection Keychain (`WhenUnlockedThisDeviceOnly`, never synced to iCloud Keychain). Never written in plaintext.

## What operators can observe

See [Privacy](privacy.md#per-app-network-routing) for the full visibility breakdown of what your ISP, SOCKS5 proxy, WireGuard peer operator, or Tor entry/exit relays can and cannot observe, and remember that clicking a link to open your system web browser uses the browser's own network settings.
