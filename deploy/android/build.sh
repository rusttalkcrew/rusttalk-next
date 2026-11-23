#!/usr/bin/env bash
# Build an APK of the RustTalk client for 64-bit ARM phones.
# Usage: deploy/android/build.sh <out-dir>
# Needs ANDROID_HOME, ANDROID_NDK_HOME, Gradle on PATH and the Rust target:
# rustup target add aarch64-linux-android
set -euo pipefail
out=$1
case ${MOBILE_BUILD:-test} in
  test)
    libdir=debug
    cargo_flags=()
    signing=(-PrusttalkTestSigning)
    apk=rusttalk-release.apk
    suffix=test
    ;;
  release)
    libdir=release
    cargo_flags=(--release)
    signing=()
    apk=rusttalk-release-unsigned.apk
    suffix=unsigned
    ;;
  *) echo "Unknown MOBILE_BUILD" >&2; exit 1 ;;
esac
api=26 # Keep in step with minSdk in build.gradle.kts.
case "$(uname -s)" in
  Linux) host=linux-x86_64 ;;
  Darwin) host=darwin-x86_64 ;;
  *) echo "Unsupported build host" >&2; exit 1 ;;
esac
bin=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$host/bin
export CC_aarch64_linux_android=$bin/aarch64-linux-android$api-clang
export CXX_aarch64_linux_android=$bin/aarch64-linux-android$api-clang++
export AR_aarch64_linux_android=$bin/llvm-ar
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$CC_aarch64_linux_android
# GameActivity loads a shared library; the crate's library builds as an rlib otherwise.
# Match the jar to the exact WebRTC archive Cargo linked, including custom builds.
webrtc_lib=$(cargo rustc "${cargo_flags[@]}" --message-format=json-render-diagnostics --locked -p rusttalk-client --lib --crate-type cdylib --target aarch64-linux-android -- \
  -C 'link-arg=-Wl,--undefined-glob=Java_livekit_*' \
  -C "link-arg=-Wl,--version-script=$PWD/deploy/android/webrtc.exports" |
  jq -r 'select(.reason == "build-script-executed" and (.package_id | contains("#webrtc-sys@"))) |
         .linked_paths[] | select(endswith("/lib")) | sub("^native="; "")')
test -f "$webrtc_lib/../libwebrtc.jar"

target=${CARGO_TARGET_DIR:-target}
shell=deploy/android
# A clean Gradle build: its incremental APK keeps the space of replaced files.
rm -rf "$shell/build"
mkdir -p "$shell/build/rust/jniLibs/arm64-v8a" "$shell/build/rust/res/mipmap" "$shell/build/rust/assets" "$out"
# Without its debug info the library shrinks from about 1 GB.
"$bin/llvm-strip" --strip-debug -o "$shell/build/rust/jniLibs/arm64-v8a/librusttalk_client.so" \
  "$target/aarch64-linux-android/$libdir/librusttalk_client.so"
cp crates/client/assets/rusttalk.png "$shell/build/rust/res/mipmap/ic_launcher.png"
cp "$webrtc_lib/../libwebrtc.jar" "$shell/build/rust/"
cp LICENSE NOTICE "$shell/build/rust/assets/"
version=$(cargo metadata --locked --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "rusttalk-client") | .version')
gradle --project-dir "$shell" --no-daemon -PrusttalkVersion="$version" "${signing[@]}" assembleRelease
cp "$shell/build/outputs/apk/release/$apk" "$out/rusttalk-$version-android-arm64-$suffix.apk"
