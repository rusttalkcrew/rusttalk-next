# Phones and tablets

RustTalk for Android and iOS is the desktop client, built from the same code:
`crates/client` and `crates/core`. There is no second app and no second UI. The
client is a library with a small `main.rs`, so Android can load it as
`librusttalk_client.so`; iOS runs the normal binary inside an app bundle.

This page is the plan (issue #282) and the state after phase 2.

## State after phase 2

On the headless emulator the Android app goes through the whole first start
with the on-screen keyboard: key password, recovery words, the word check,
name, then joining a local server by invite link, creating a channel, sending
messages and reading them again after a restart.

- **Typing** (`src/android.rs`). The keyboard doesn't send keys to
  GameActivity: it edits a text buffer GameActivity keeps, and winit 0.30 drops
  the events that say it changed. Each frame reads the buffer and turns what
  changed into egui text, IME composition (autocorrect, Japanese input),
  deletions and Enter. While a field is focused the app paints every 40 ms to
  look, since nothing else wakes it. Checked: letters, backspace, Enter,
  autocorrect replacing a word, and the buffer starting over after a sent
  message. Gboard on the emulator sends no composing region for English, so
  composition and Japanese input are covered by unit tests only.
- **Keyboard type**: the field's purpose sets the keyboard: password fields get
  no suggestions and ask the keyboard not to learn (`IME_FLAG_NO_PERSONALIZED_LEARNING`).
  Every field asks for an Enter key instead of an action key, since winit drops
  editor actions.
- **Safe area**: system bars, display cutout and the keyboard come from
  GameActivity's window insets (read on the Java thread). The app draws inside
  what is left, so the focused field sits above the keyboard and is scrolled
  into view when the keyboard opens. The keyboard hides when focus leaves a
  field (egui and winit already did that).
- **Recovery words** show in two columns on phones.
- **Destroy**: the app runs eframe on its own winit loop and ends it once
  android-activity has handled `onDestroy`, then ends the process (see Android
  lifecycle).

## State after phase 1

- `rusttalk-client` and `rusttalk-core` build for `aarch64-linux-android`,
  LiveKit and WebRTC included. `deploy/android/build.sh` makes a debug APK.
- On a headless Android 15 emulator (x86_64 image running the arm64 build) the
  APK starts, draws the key storage screen, takes taps and opens the
  on-screen keyboard. (An earlier build that still offered the protected file
  also reached the welcome screen and the recovery words.)
- The APK is a debug build: debuggable, so anyone with the phone and a USB cable
  can read the app's files. Use test accounts only.
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
| Text input | GameActivity's text buffer, read each frame (`src/android.rs`) | winit IME | Android works (phase 2) |
| Voice and video (LiveKit, webrtc-sys) | Builds and links (prebuilt WebRTC for Android) | Prebuilt WebRTC for iOS; CI builds it | Untested at runtime; see Voice |
| Audio (cpal 0.18) | AAudio (`libaaudio.so`, Android 8.0+) | Core Audio | Builds; needs mic permission and audio session |
| Key storage (keyring 4) | No Android store | The data-protection Keychain store builds, not used yet | File with password only (see Privacy and permissions) |
| File picker (rfd) | No backend | No backend | Gated: the buttons that open it are off (`files::PICKER`) |
| Notifications (notify-rust) | Builds, no notification service | Builds, no notification service | Fail and are logged |
| Camera (nokhwa) | No backend | No backend | Listing cameras fails, so no camera |
| Screen share, app and system audio | Not offered (`screen` already says unavailable) | Same | App audio gated to `Unsupported` |
| Updater | Not used | Not used | Gated: no update target, so no checks and no **Check now** |
| Global shortcuts (global-hotkey) | Builds, nothing to register | Same | Unused; Shortcuts tab still shows |
| Remote control | Not offered | Not offered | Unchanged (only Linux and macOS offer it) |
| Clipboard | Text through egui only | Same | Picture and file paste gated off |
| Deep links (`rusttalk://`) | Not registered | Not registered | Not started (desktop has none either) |
| Settings folder | `AndroidApp::internal_data_path()`, no backup or transfer | Application Support, excluded from backups | Done |

### Android lifecycle

winit 0.30 ignores `onDestroy` (a TODO in its Android backend), and
GameActivity waits for `android_main` to return before the activity, or a new
one, can go on. The app therefore runs eframe on its own winit loop
(`eframe::create_native`) and ends that loop once android-activity has handled
`onDestroy` (its app folder is then gone). winit 0.30 also makes only one event
loop per process, so `android_main` then ends the process (`_exit`; the normal
exit handlers call into Java from the wrong thread) and Android starts a fresh
one for the next activity. Checked on the emulator by starting the app again
with `FLAG_ACTIVITY_CLEAR_TASK`: the old process ends within a second, the new
one shows the unlock screen, no "app not responding".

The activity still takes every configuration change itself (`configChanges` in
the manifest): turning the phone, a new font size, language or dark mode keep
the same activity instead of restarting the process.

### Voice

WebRTC's Android code calls into Java. Before a call works on Android:

1. `livekit::webrtc::android::initialize_android_context` runs on a thread
   that Java called into, through a native method the app's Java side calls
   (for example from the activity's `onCreate`). On the `android_main` thread
   it fails at `FindClass("livekit/org/webrtc/ContextUtils")`: native threads
   have no app class loader. Keep GameActivity's own JNI registration as it
   is; add the method next to it, not a `JNI_OnLoad` that replaces it.
2. Check the boolean it returns, and log and turn voice off when it is false.
3. `libwebrtc.jar`, which `webrtc-sys` copies next to the build, goes into the
   APK through Gradle.
4. The app asks for the microphone (`RECORD_AUDIO`) when a person first joins
   a voice channel, and sets Android's communication audio mode.

On iOS the audio session needs `AVAudioSession` set to play and record for
voice chat before the call starts, and the background audio mode keeps a
call going with the screen off. Test microphone and playback on a real
iPhone in that phase; the simulator's audio says little about a device.

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
- **Dialogs:** full width; some (Create channel) are wider than a phone.
- **Touch targets:** phones get at least 44 points (Apple) / 48 dp (Android)
  for rows and icon buttons, set once in `ui::install` from the same
  breakpoint, not per screen.
- **Hover:** tooltips and hover-only message actions need a long press
  instead (egui turns a long press into a secondary click, which already
  opens context menus).
- **Back:** Android's back gesture should do what the Back bar does.

## Later phases

Rough effort for one person who knows the code base.

| Phase | What | Effort |
| --- | --- | --- |
| 2 | Usable chat on Android. Done: typed text, safe areas and the keyboard, `onDestroy`, recovery words. Left: phone temp folder (`temp_dir()` is not writable on Android), system TLS roots for non-pinned hosts (check, then rustls-platform-verifier), file picker (Storage Access Framework), local notifications, Android Keystore for the keys, hide Updates and Shortcuts on phones, `rusttalk://` links, the layout list above | 3 to 4 weeks |
| 3 | Voice on Android and iOS: WebRTC init, `libwebrtc.jar`, mic permission, audio session and Bluetooth headsets, calls with the screen off (foreground service on Android, background audio on iOS) | 2 weeks |
| 4 | iOS on a device: Keychain, notifications (reuse `notify/macos.rs`), document picker, safe areas, background rules | 1 to 2 weeks |
| 5 | Camera (Camera2, AVFoundation) | 1 week |
| 6 | Release: signing keys, store listings or a sideload page, release.yml jobs, store updates | 1 week plus store review |
| Later | Screen share (MediaProjection; ReplayKit broadcast extension), push notifications | 2 to 4 weeks each |

## Privacy and permissions

- **No new connections.** The phone apps connect to what the desktop client
  does (see [Privacy](privacy.md#outbound-connections)). The updater makes
  none on phones. AppCompat's EmojiCompat would ask Google Play services for
  an emoji font at start; the manifest removes its start-up entry, so the
  APK never makes that request. Building downloads Gradle, the Android Gradle plugin,
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
  Android: `allowBackup="false"` and data extraction rules that exclude
  everything, so nothing goes to Google's cloud or to a new phone. iOS: the
  folder is Application Support, marked excluded from backups
  (`NSURLIsExcludedFromBackupKey`) before the key storage opens, so nothing
  goes to iCloud or a computer.
- **Keys:** phones offer only "File with password" (`keystore::PASSWORD_ONLY`,
  see [Crypto](crypto.md#local-identity)) until phase 2 uses Android Keystore
  and phase 4 the iOS Keychain. The iOS build already includes keyring's
  data-protection Keychain store (`apple-native-keyring-store`, `protected`),
  which can keep an item with `when-unlocked-this-device-only` access: never
  in a backup or on another device. Using it needs a small keystore backend
  of our own (keyring's `v1` API has no iOS store) and a test on a signed
  device, so it waits for phase 4. A key file found without a password, or a
  profile recorded as the plain file, first asks for a password; existing keys
  are then locked with it in one atomic replace before the account opens, and
  no plain key file is ever written. A key file is never refused or
  destroyed: writing other keys over it (recovery words on a locked profile,
  a new account after a cut-short save) first moves it to
  `identity.key.replaced-<unix time>`, a file without a password locked with
  the new one on the way, and writing the same keys again only
  finishes what was missing. Those files are kept until sign-out removes them;
  they can also be deleted by hand once the account works. The protected file without
  a password is off on phones: a debuggable test build, a rooted phone or a
  backup tool that ignores the rules above would hand over readable keys.
  Zero-knowledge is unchanged: phones run the same `crates/core` crypto.
