#!/usr/bin/env bash
# Package the release client and its PipeWire helper as an AppImage.
# Usage: appimage.sh <version> <arch: x86_64|aarch64> <appimagetool> <runtime> <out-dir>
# Run after `cargo build --release --locked -p rusttalk-client -p rusttalk-pipewire`.
set -euo pipefail
version=$1 arch=$2 tool=$3 runtime=$4 out=$5
target=${CARGO_TARGET_DIR:-target}/release
appdir=$(mktemp -d)/RustTalk.AppDir
mkdir -p "$appdir/usr/bin" "$out"
# Both binaries must sit in one directory for sibling helper discovery.
cp "$target/rusttalk-client" "$target/rusttalk-pipewire" "$appdir/usr/bin/"
ln -s usr/bin/rusttalk-client "$appdir/AppRun"
cp deploy/release/rusttalk.desktop deploy/release/rusttalk.svg "$appdir/"
# Apache-2.0 requires the license and NOTICE in every copy.
mkdir -p "$appdir/usr/share/doc/rusttalk"
cp LICENSE NOTICE "$appdir/usr/share/doc/rusttalk/"
# A pinned local runtime: appimagetool would otherwise download one unverified.
ARCH=$arch APPIMAGE_EXTRACT_AND_RUN=1 "$tool" --runtime-file "$runtime" "$appdir" \
  "$out/rusttalk-$version-linux-$arch.AppImage"
