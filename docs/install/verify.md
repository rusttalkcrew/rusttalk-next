# Check that a download is genuine

1. Download only from
   [github.com/rusttalkcrew/rusttalk-next/releases](https://github.com/rusttalkcrew/rusttalk-next/releases).
   The address bar must show exactly that.
2. Compare your file's SHA-256 checksum with the one GitHub shows next to the
   file on the release page, or with the one in the release's `latest.json`
   ([latest version](https://github.com/rusttalkcrew/rusttalk-next/releases/latest/download/latest.json)).
   They must match.

Get your file's checksum:

- **Windows** (PowerShell): `Get-FileHash .\RustTalk-windows-x86_64.exe`
- **Linux:** `sha256sum RustTalk-linux-x86_64.AppImage`
- **macOS:** `shasum -a 256 RustTalk-macos-universal.dmg`

`latest.json` lists one checksum per platform: `windows-x86_64`,
`windows-aarch64`, `linux-x86_64`, `linux-aarch64`, `macos-aarch64` and
`macos-x86_64`. The two macOS entries are the same file.

If the checksums differ, delete the file and download it again. Still
different? [Open an issue](https://github.com/rusttalkcrew/rusttalk-next/issues).

This catches a damaged or swapped file, but both checksums come from GitHub.
Once RustTalk is installed, it only accepts updates that are signed with the
release key built into the app and whose checksum matches.
