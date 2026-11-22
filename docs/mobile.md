# Phones and tablets

RustTalk for Android and iOS is the desktop client, built from the same code:
`crates/client` and `crates/core`. There is no second app and no second UI. The
client is a library with a small `main.rs`, so Android can load it as
`librusttalk_client.so`; iOS runs the normal binary inside an app bundle.

This page is the plan (issue #282) and the state after phase 1.

## State after phase 1

- `rusttalk-client` and `rusttalk-core` build for `aarch64-linux-android`,
  LiveKit and WebRTC included. `deploy/android/build.sh` makes a debug APK.
- On a headless Android 15 emulator (x86_64 image running the arm64 build) the
  APK starts, draws the key storage screen, the welcome screen and the
  recovery words, takes taps and opens the on-screen keyboard.
- Typed text does not reach text fields yet (see Phase 2). Nothing past account
  creation was tried on a device.
- `deploy/ios/build.sh` makes an unsigned IPA. iOS can't be checked from Linux:
  `ring`, `cxx`, `webrtc-sys` and `coreaudio-sys` compile C, C++ and bindings
  against the iOS SDK through `xcrun`. The macOS runner in
  `.github/workflows/mobile.yml` is the real iOS check.
- Phone-sized windows (narrower than `ui::PHONE_WIDTH`, 600 points) show the
  server and channel list or what is open, never both, with a Back bar; the
  member list covers the column. Desktop windows can't get that narrow
  (minimum 760), so desktop looks the same.

## Subsystems

"Gated" means `cfg(any(target_os = "android", target_os = "ios"))` picks a
small stand-in; desktop code is unchanged. Desktop-only code that phones don't
use yet is allowed to be unused there (`allow(dead_code)` on phones only).

| Subsystem | Android | iOS | Phase 1 |
| --- | --- | --- | --- |
| Window, input, drawing (eframe 0.37) | GameActivity through android-activity 0.6; drawn with glow (OpenGL ES) | winit's UIKit backend, wgpu on Metal | Builds. Android runs. |
| Text input | GameActivity shows the keyboard; winit 0.30 drops its text | winit IME | Android: broken, phase 2 blocker |
| Voice and video (LiveKit, webrtc-sys) | Builds and links (prebuilt WebRTC for Android) | Prebuilt WebRTC for iOS; CI builds it | Untested at runtime; see Voice |
| Audio (cpal 0.18) | AAudio (`libaaudio.so`, Android 8.0+) | Core Audio | Builds; needs mic permission and audio session |
| Key storage (keyring 4) | No Android store | keyring has an Apple store, not enabled | Falls back to the existing Protected file / File with password choice |
| File picker (rfd) | No backend | No backend | Gated: picking finds nothing |
| Notifications (notify-rust) | Builds, no notification service | Builds, no notification service | Fail and are logged |
| Camera (nokhwa) | No backend | No backend | Listing cameras fails, so no camera |
| Screen share, app and system audio | Not offered (`screen` already says unavailable) | Same | App audio gated to `Unsupported` |
| Updater | Not used | Not used | Gated: no update target, so no checks and no **Check now** |
| Global shortcuts (global-hotkey) | Builds, nothing to register | Same | Unused; Shortcuts tab still shows |
| Remote control | Not offered | Not offered | Unchanged (only Linux and macOS offer it) |
| Clipboard | Text through egui only | Same | Picture and file paste gated off |
| Deep links (`rusttalk://`) | Not registered | Not registered | Not started (desktop has none either) |
| Settings folder | `AndroidApp::internal_data_path()` | Application Support in the app sandbox | Done |

### Voice

WebRTC's Android code calls into Java. Before a call works on Android:

1. `JNI_OnLoad` in the library calls
   `livekit::webrtc::android::initialize_android`, and `android_main` passes
   the activity to `initialize_android_context`. Doing this from
   `android_main` alone is not enough: that thread has no app class loader.
2. `libwebrtc.jar`, which `webrtc-sys` copies next to the build, goes into the
   APK through Gradle.
3. The app asks for the microphone (`RECORD_AUDIO`) when a person first joins
   a voice channel, and sets Android's communication audio mode.

On iOS the audio session needs `AVAudioSession` set to play and record for
voice chat before the call starts, and the background audio mode keeps a
call going with the screen off.

Until then joining a call on a phone is expected to fail.

## Build tools

**Android: plain `cargo` with the NDK's clang, plus a small Gradle shell.**
`deploy/android/build.sh` points the C compilers and linker at the NDK (what
cargo-ndk automates), builds the client library as a `cdylib` with
`cargo rustc --crate-type cdylib`, strips its debug info (1 GB to about
240 MB) and lets Gradle package it in `deploy/android/`.

- **cargo-apk** is deprecated and only does NativeActivity, which has no
  on-screen keyboard support and no AccessKit in eframe.
- **xbuild** is unmaintained and has its own packaging, with no simple way to
  add Android libraries (AARs).
- **Gradle** is needed anyway: GameActivity ships as an AAR, and later phases
  need Java or Kotlin glue (notifications, Android Keystore, the file picker,
  `libwebrtc.jar`). The shell has no Java code yet: the manifest starts
  `com.google.androidgamesdk.GameActivity` directly.

cargo-ndk can replace the few lines of environment in `build.sh` when more
ABIs (32-bit ARM, x86_64) are added.

**iOS: plain `cargo build` and a hand-made bundle.** `deploy/ios/build.sh`
builds the binary for `aarch64-apple-ios`, puts it in `Payload/RustTalk.app`
with `deploy/ios/Info.plist`, the icon, LICENSE and NOTICE, and zips it into
an unsigned IPA. That is what cargo-bundle does, without one more tool.
AltStore, Sideloadly and similar tools sign the IPA with the person's own
Apple account. An Xcode project (xcodebuild) comes with signing,
entitlements and the App Store.

### Building locally

```sh
rustup target add aarch64-linux-android
ANDROID_HOME=… ANDROID_NDK_HOME=… deploy/android/build.sh dist
```

Needs the Android SDK, NDK r29 (`ndk;29.0.14206865`), Gradle 9.8 or newer,
Java 17 or newer and `jq`. On a Mac with Xcode:

```sh
rustup target add aarch64-apple-ios
deploy/ios/build.sh dist
```

CI builds both on pull requests that touch the client, and on request from
the Actions tab (`Mobile apps`), and keeps the APK and IPA as artifacts. It is
not part of releases yet.

## Phone layout

`ui::is_phone` is the one breakpoint: below 600 points the main layout is a
single column, in this order: server list and channels, then the open
channel, with the member list as a cover over it and Back to return. Picking a
channel, DM or page opens it.

Still to do, screen by screen:

- **Call panel and voice channel:** the call and its chat stacked, controls in
  a bottom bar, the participant list as a cover like the member list.
- **Settings:** the page list, then the page, like the main layout.
- **Dialogs:** full width with the keyboard in mind; the recovery words grid
  has four fixed columns and is cut off on phones.
- **Touch targets:** phones get at least 44 points (Apple) / 48 dp (Android)
  for rows and icon buttons, set once in `ui::install` from the same
  breakpoint, not per screen.
- **Hover:** tooltips and hover-only message actions need a long press
  instead (egui turns a long press into a secondary click, which already
  opens context menus).
- **Safe areas:** the notch, status bar and gesture bar are drawn over today.
- **Back:** Android's back gesture should do what the Back bar does.

## Later phases

Rough effort for one person who knows the code base.

| Phase | What | Effort |
| --- | --- | --- |
| 2 | Usable chat on Android: typed text (forward GameActivity's text state to egui, or move to winit 0.31), safe areas, phone temp folder (`temp_dir()` is not writable on Android), system TLS roots for non-pinned hosts (check, then rustls-platform-verifier), file picker (Storage Access Framework), local notifications, Android Keystore for the keys, hide Updates and Shortcuts on phones, `rusttalk://` links, the layout list above | 3 to 4 weeks |
| 3 | Voice on Android and iOS: WebRTC init, `libwebrtc.jar`, mic permission, audio session and Bluetooth headsets, calls with the screen off (foreground service on Android, background audio on iOS) | 2 weeks |
| 4 | iOS on a device: Keychain, notifications (reuse `notify/macos.rs`), document picker, safe areas, background rules | 1 to 2 weeks |
| 5 | Camera (Camera2, AVFoundation) | 1 week |
| 6 | Release: signing keys, store listings or a sideload page, release.yml jobs, store updates | 1 week plus store review |
| Later | Screen share (MediaProjection; ReplayKit broadcast extension), push notifications | 2 to 4 weeks each |

## Privacy and permissions

- **No new connections.** The phone apps connect to what the desktop client
  does (see [Privacy](privacy.md#outbound-connections)). The updater makes
  none on phones. Building downloads Gradle, the Android Gradle plugin,
  GameActivity and AppCompat from Google's Maven repository, next to the
  WebRTC archive the desktop build already downloads.
- **Push notifications** (when the app is closed) would need Firebase Cloud
  Messaging or Apple Push and a relay that knows device tokens. That is a new
  outbound connection and a privacy decision; it is not planned until then.
  Local notifications while the app runs need neither.
- **Android permissions:** phase 1 asks for `INTERNET` only. Later, each
  asked for when first needed: `RECORD_AUDIO` (joining voice), `CAMERA`
  (turning video on), `POST_NOTIFICATIONS` (Android 13+, turning
  notifications on), foreground service for calls, `BLUETOOTH_CONNECT` for
  headsets. Never location, contacts or phone state.
- **iOS:** the bundle states why it uses the microphone and camera; iOS asks
  the person on first use.
- **Storage:** settings and the key file live in the app's private storage.
  Android backups are off (`allowBackup="false"`), so they don't go to
  Google's cloud. iOS backups include Application Support: phase 4 moves the
  keys to the Keychain (this device only) or excludes the key file.
- **Keys:** until phase 2 and 4 phones use the existing protected file or
  password file (see [Crypto](crypto.md#local-identity)). Zero-knowledge is
  unchanged: phones run the same `crates/core` crypto.
