#!/usr/bin/env bash
# Build an unsigned RustTalk IPA for iPhone and iPad, to sign when sideloading.
# Usage: deploy/ios/build.sh <out-dir>   (macOS with Xcode)
# Needs the Rust target: rustup target add aarch64-apple-ios
set -euo pipefail
out=$1
# Matches MinimumOSVersion in the bundle's Info.plist.
export IPHONEOS_DEPLOYMENT_TARGET=16.0
case ${MOBILE_BUILD:-test} in
  test) profile=dev; bindir=debug ;;
  release) profile=release; bindir=release ;;
  *) echo "Unknown MOBILE_BUILD" >&2; exit 1 ;;
esac
cargo build --profile "$profile" --locked -p rusttalk-client --target aarch64-apple-ios
target=${CARGO_TARGET_DIR:-target}
version=$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "rusttalk-client") | .version')

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
app=$work/Payload/RustTalk.app
mkdir -p "$app" "$out"
cp "$target/aarch64-apple-ios/$bindir/rusttalk-client" "$app/"
sed -e "s/@VERSION@/$version/g" -e "s/@MINIMUM@/$IPHONEOS_DEPLOYMENT_TARGET/g" \
  deploy/ios/Info.plist > "$app/Info.plist"
plutil -lint "$app/Info.plist"
# Apache-2.0 requires the license and NOTICE in every copy.
cp LICENSE NOTICE "$app/"
sips -z 120 120 crates/client/assets/rusttalk.png --out "$app/AppIcon60x60@2x.png" >/dev/null
sips -z 180 180 crates/client/assets/rusttalk.png --out "$app/AppIcon60x60@3x.png" >/dev/null
# No signature: AltStore, Sideloadly and similar tools sign it with the person's own
# Apple account when they install it.
(cd "$work" && zip -qr "rusttalk.ipa" Payload)
mv "$work/rusttalk.ipa" "$out/rusttalk-$version-ios-unsigned.ipa"
