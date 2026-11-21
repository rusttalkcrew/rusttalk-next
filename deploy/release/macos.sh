#!/usr/bin/env bash
# Build the universal (Apple Silicon and Intel) RustTalk.app and package it as a DMG.
# Usage: macos.sh <version> <out-dir>
# Needs both Rust targets: rustup target add aarch64-apple-darwin x86_64-apple-darwin
set -euo pipefail
version=$1 out=$2
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || { echo "Invalid version $version" >&2; exit 1; }
# Test builds accept local update servers; release packages never do.
unset RUSTTALK_LOCAL_UPDATE_TEST
# Matches LSMinimumSystemVersion in Info.plist (ScreenCaptureKit needs 14.2).
export MACOSX_DEPLOYMENT_TARGET=14.2
target=${CARGO_TARGET_DIR:-target}
for arch in aarch64 x86_64; do
  cargo build --release --locked -p rusttalk-client --target "$arch-apple-darwin"
done

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app=$work/dmg/RustTalk.app
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$out"
lipo -create -output "$app/Contents/MacOS/rusttalk-client" \
  "$target/aarch64-apple-darwin/release/rusttalk-client" \
  "$target/x86_64-apple-darwin/release/rusttalk-client"
sed "s/@VERSION@/$version/g" deploy/release/Info.plist > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"
# Apache-2.0 requires the license and NOTICE in every copy.
cp LICENSE NOTICE "$app/Contents/Resources/"

iconset=$work/rusttalk.iconset
mkdir "$iconset"
# The source icon is 256 px, so the largest entry is 256 (and 128@2x).
for entry in 16x16:16 16x16@2x:32 32x32:32 32x32@2x:64 128x128:128 128x128@2x:256 256x256:256; do
  size=${entry#*:}
  sips -z "$size" "$size" crates/client/assets/rusttalk.png --out "$iconset/icon_${entry%:*}.png" >/dev/null
done
iconutil -c icns -o "$app/Contents/Resources/rusttalk.icns" "$iconset"

# Ad-hoc signature: Apple Silicon refuses to run unsigned code. No Developer ID yet.
codesign --force --sign - "$app"
codesign --verify --strict --verbose=2 "$app"
lipo "$app/Contents/MacOS/rusttalk-client" -verify_arch arm64 x86_64

ln -s /Applications "$work/dmg/Applications"
dmg=$out/rusttalk-$version-macos-universal.dmg
# hdiutil sometimes reports "Resource busy" on CI runners; a retry clears it.
for attempt in 1 2 3; do
  hdiutil create -volname RustTalk -srcfolder "$work/dmg" -fs HFS+ -format UDZO -ov "$dmg" && break
  [[ $attempt -lt 3 ]] || exit 1
  sleep 5
done
hdiutil verify "$dmg"
