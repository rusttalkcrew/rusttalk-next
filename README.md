# RustTalk

Private, self-hosted voice, video and chat for friends and small communities.
Everything people write or name is encrypted on their device; servers only store
and forward ciphertext.

Status: early development. See [docs/work-order.md](docs/work-order.md) for progress.

## Download

[![Windows x64](https://img.shields.io/badge/Windows-x64-0078D4?style=for-the-badge&logo=windows&logoColor=white)](https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/RustTalk-windows-x86_64.exe)
[![Windows ARM64](https://img.shields.io/badge/Windows-ARM64-0078D4?style=for-the-badge&logo=windows&logoColor=white)](https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/RustTalk-windows-aarch64.exe)
[![Linux x64](https://img.shields.io/badge/Linux-x64-FCC624?style=for-the-badge&logo=linux&logoColor=white)](https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/RustTalk-linux-x86_64.AppImage)
[![Linux ARM64](https://img.shields.io/badge/Linux-ARM64-FCC624?style=for-the-badge&logo=linux&logoColor=white)](https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/RustTalk-linux-aarch64.AppImage)
[![macOS](https://img.shields.io/badge/macOS-Apple%20Silicon%20%26%20Intel-000000?style=for-the-badge&logo=apple&logoColor=white)](https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/RustTalk-macos-universal.dmg)

Windows installs per user without admin rights. On Linux, make the AppImage
executable (`chmod +x RustTalk-*.AppImage`) and run it. On macOS (14.2 or
later), open the DMG and drag RustTalk to Applications. All three update
themselves. All releases: [GitHub releases](https://github.com/rusttalkcrew/rusttalk-next/releases).

### Opening RustTalk on macOS

RustTalk isn't signed by Apple yet, so the first time macOS says it can't
check the app. Right-click RustTalk in Applications, choose **Open**, then
**Open** again. On macOS 15 or later, open **System Settings → Privacy &
Security** after that first try and click **Open Anyway**. Or run this once in
Terminal:

```sh
xattr -dr com.apple.quarantine /Applications/RustTalk.app
```

## Quick start

Requires Docker with Compose and a Rust toolchain. Run `make` to see every
target. Start the stack in one terminal and run the health check in another.
Use Ctrl-C to stop watching for changes before shutting down the stack.

```sh
make dev
curl -fsS --cacert .dev/tls/cert.pem https://127.0.0.1:8443/v1/health
make down
```

## Run a server

On a Linux host with Docker, a public IPv4 address and ports 443/tcp,
7881/tcp and 7882/udp open:

```sh
git clone https://github.com/rusttalkcrew/rusttalk-next.git
cd rusttalk-next/deploy
cp .env.example .env    # set RUSTTALK_IP, LIVEKIT_API_SECRET, optionally RUSTTALK_DOMAIN
docker compose pull && docker compose up -d --wait
```

Already running Caddy, nginx or Traefik on 443? Use
[behind your own reverse proxy](docs/operations.md#behind-your-own-reverse-proxy).
Owner invite, backups and upgrades: [Production](docs/operations.md#production).
Optional automatic patch updates (off by default, no backup or rollback):
[Automatic updates](docs/operations.md#automatic-updates). Running version:
`curl https://<host>/v1/server-info`.

## Docs

- [Architecture](docs/architecture.md)
- [Protocol](docs/protocol.md)
- [Crypto](docs/crypto.md)
- [Privacy](docs/privacy.md)
- [Operations](docs/operations.md)
- [Development](docs/development.md)
- [Work order](docs/work-order.md)

License: [Apache-2.0](LICENSE). If you reuse or redistribute RustTalk, keep the [NOTICE](NOTICE) file, which credits the original authors. Contributing: see [CONTRIBUTING.md](CONTRIBUTING.md). The bundled [Inter](https://rsms.me/inter/) font is under the [SIL Open Font License 1.1](crates/client/assets/fonts/Inter-LICENSE.txt), and so is the bundled Japanese subset of [Noto Sans CJK](https://github.com/notofonts/noto-cjk) ([license](crates/client/assets/fonts/NotoSansJP-LICENSE.txt)).
[Twemoji](https://github.com/jdecked/twemoji) graphics by Twitter, Inc. and other
contributors are licensed under [CC BY 4.0](crates/client/assets/emoji/TWEMOJI-LICENSE.txt).
The bundled [Emojibase data](crates/client/assets/emoji/README.md) is under the
[MIT license](crates/client/assets/emoji/EMOJIBASE-LICENSE.txt).
