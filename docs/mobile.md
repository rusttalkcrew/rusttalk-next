# Phones and tablets

RustTalk for Android and iOS is the desktop client, built from the same code:
`crates/client` and `crates/core`. There is no second app and no second UI. The
client is a library with a small `main.rs`, so Android can load it as
`librusttalk_client.so`; iOS runs the normal binary inside an app bundle.

## Install and update

Download the experimental phone packages from [RustTalk releases](https://github.com/rusttalkcrew/rusttalk-next/releases).

- **Android 8 or newer, ARM64:** open `RustTalk-android-arm64.apk` and allow installation from that source. Install later releases over the existing app. Actions test APKs now install next to it as "RustTalk Test"; older test APKs used the release package name with a different signer and block the release APK. Before removing a test install, check that you have your recovery words. Uninstalling clears the app's local data.
- **iOS 16 or newer:** sign `RustTalk-ios-unsigned.ipa` with your Apple account using a sideloading tool such as AltStore or xtool. Use the same signing team for updates so the app can still read your keys in Keychain.

### "App not installed" on Android

Android shows only "App not installed". To see the reason, turn on USB
debugging, connect the phone and run `adb install -r RustTalk-android-arm64.apk`
([adb](https://developer.android.com/tools/releases/platform-tools)):

- `INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package
  io.github.rusttalkcrew.rusttalk signatures do not match newer version`:
  another copy with a different signer is installed. Usually it is a test
  APK from a pull request's CI run: test builds are signed with a different
  key and can never be upgraded to a release. The copy can be hidden: missing
  from the launcher, in a work profile, Private Space or Secure Folder, or
  uninstalled with "keep data". `adb shell dumpsys package io.github.rusttalkcrew.rusttalk`
  shows which users have it (`User <id>: ... installed=true`). After saving
  your recovery words, remove it with `adb uninstall io.github.rusttalkcrew.rusttalk`
  (all users) or `adb uninstall --user <id> io.github.rusttalkcrew.rusttalk`,
  then install again. Every release so far is signed with the same key, so
  releases always update each other.
- `INSTALL_FAILED_VERSION_DOWNGRADE`: a newer RustTalk is installed.
- `INSTALL_FAILED_NO_MATCHING_ABIS`: the phone is not 64-bit ARM.
- `INSTALL_FAILED_OLDER_SDK`: the phone runs Android older than 8.
- `INSTALL_FAILED_INSUFFICIENT_STORAGE`: free up space; the APK is over 100 MB.
- Succeeds over adb but not from the file manager: Play Protect or the
  file manager blocked it; allow the install when Play Protect asks.

Check that the APK is genuine with [these steps](install/verify.md).

Phone updates are manual. Microphone audio and camera capture still need verification on a physical iPhone.

## iPhone (phase 4, started)

The unsigned IPA, signed and installed with xtool, starts on a real iPhone.

- **Safe area and taps** (`src/ios.rs`). winit 0.30 reports the window without
  the notch and home indicator as its size (`inner_size`), and egui-wgpu sizes
  the Metal drawable from that, while the Metal layer, touches and egui's screen
  (`outer_size`) cover the whole screen. The layer stretched the smaller drawable
  over the screen, so things showed further down than where taps landed. Each
  frame the app moves the Metal layer onto the safe area, where the drawable
  fits it exactly, gives egui that area as its screen and moves touches into it.
  VoiceOver gets the same shift through the AccessKit root node's transform
  (accesskit_ios places nodes and hit-tests in the whole-screen view). The
  status bar and home indicator areas show the theme's background (the window's
  and winit's view's `backgroundColor`), and the status bar text is light on
  dark themes and dark on light ones (winit's `set_preferred_status_bar_style`).
  Once per run the log records the window, safe area, drawable size and scales
  (`iOS window`).
- **Keyboard** (`src/ios.rs`). Each frame reads UIKit's keyboard layout guide;
  how far the docked keyboard reaches into the safe area becomes egui's bottom
  safe-area inset, and the app draws above it, as on Android, so pages scroll
  and the focused field is scrolled to the middle of what is left (the button
  under it shows too). Frames keep coming every 100 ms while a field has focus
  and for a moment after the keyboard moved; the log notes each change (`iOS
  keyboard`, in points). winit sends Return as the text "\n", which egui-winit
  drops, so the app replaces winit's view's `insertText:` once with one that
  notes a Return and then calls winit's: Return reaches egui as Enter, which
  ends a one-line field (closing the keyboard) and is a new line in a text box.
  Tapping outside the field ends the focus and closes the keyboard. Phones never
  give a field focus by themselves (desktops focus a screen's first field), as
  that would open the keyboard again right after a tap closed it.
- **Keys in the Keychain** (`keystore.rs`, `IosKeychain`). A new account's keys
  go to the data-protection Keychain, readable only while the phone is unlocked
  and never in iCloud Keychain (`WhenUnlockedThisDeviceOnly`). An encrypted
  backup can hold the item, but it restores only to this same iPhone, never to
  another one. (`WhenPasscodeSetThisDeviceOnly` would also keep it out of
  backups, but iOS deletes such items when the passcode is turned off, losing
  the keys without a word.) The item lives in the app's default access
  group, which comes from the signing team; a build signed without one can't
  write there. The app writes and removes a test item once per run first: when
  that fails it says the Keychain isn't available and asks for a password, as on
  Android. Keys already in a password file keep opening with it, and Settings >
  Privacy > Key storage moves them to the Keychain and back (the desktop switch).
  The protected file without a password is never offered. Signing the app again
  with another Apple account (another team) hides the Keychain item: the app
  then shows the missing-keys screen, and the recovery words bring the account
  back. Keychain items outlive the app, so a fresh install (no `keystore.json`,
  no account or key files) first deletes a RustTalk item an earlier install
  left, and starts with sign-up or the recovery words like a new phone; a
  damaged `keystore.json` is not a fresh install and keeps its keys.

## State after phase 3

Android can join foreground voice calls. Joining asks for microphone access;
permission continues the pending join, and denial leaves the person out of
the call with a short message. Calls use the same encrypted microphone and
playback paths as desktop. On the headless Android 15 emulator, denial showed
the microphone message; granting permission joined the call with a desktop
peer. Both published GCM-encrypted audio tracks, and leaving restored normal
audio mode. Host audio was disabled; audible playback still needs a device.

## State after phase 2

On the headless emulator the Android app goes through the whole first start
with the on-screen keyboard: key password, recovery words, the word check,
name, then joining a local server by invite link, creating a channel, sending
messages and reading them again after a restart.

- **Typing** (`src/android.rs`). The keyboard doesn't send keys to
  GameActivity: it edits a text buffer that GameTextInput keeps, and winit 0.30
  drops the events that say it changed. For a normal field the buffer starts
  with its text for up to 100 characters each side of the cursor (at most 1,000
  in all; a longer selection starts empty and is replaced), also right after the
  field takes focus, so the keyboard can delete and correct what is already
  there. Password and recovery-word fields never give the keyboard their text:
  the field says it is secret before anything is shared, its buffer starts
  empty, and on focus the cursor goes to the end, as in Android password fields
  (Backspace deletes from there). Each change to the buffer (text, selection, composed word) becomes the same
  change at the same place in the field, through the field's cursor and egui
  text and delete events. Moving the cursor in the field starts the buffer over
  there. Reading and setting the buffer happen on the Java thread, where the
  keyboard edits it, through GameTextInput's C API. A start-over is numbered and
  counts only once the Java thread reads back exactly what was set; until then
  the field ignores the buffer, and the start-over is sent again every 300 ms.
  A composed word shows as plain text (egui can't mark a range from outside).
  While a field is focused the app paints every 40 ms to read the buffer, since
  nothing else wakes it.
- **Enter**: winit drops the keyboard's action key, so `MainActivity` (the only
  Java in the app) counts presses of Done (not Next or Search) and the app turns
  each into one Enter for the field that had focus when it came. The key shows
  Done. Newlines are text, also a lone one from Enter on a hardware
  keyboard or a paste, so nothing is sent by accident.
- **Clipboard** (`src/phone_clipboard.rs`): egui-winit's clipboard is only a
  string inside the app on phones, so copies go to Android's `ClipboardManager`
  (on the Java thread) or iOS's `UIPasteboard` instead, and pastes come from
  there. Pressing and holding a text field opens Paste, Copy, Cut and Select
  all; Copy and Cut only with a selection, and never in password or
  recovery-word fields. A paste moves the field's cursor, which starts the
  keyboard's buffer over with the pasted text. While the menu is open, picking
  a row keeps the field focused, so the keyboard stays up. A tap on the cursor
  of a field that already has focus opens the menu too, as on phones. While a
  finger rests on the screen the app draws a frame every 100 ms, as iOS sends
  no touches then and egui only sees a press and hold on a frame after it. On
  iOS the log notes each tap and long press on a field (how long it was held,
  never any text: `text field touch`), to check this on a device.
- **Tapping a focused field** asks egui for focus again, which also asks to
  interrupt the keyboard's typing; egui-winit does that by turning the
  keyboard off and on (winit hides and shows Android's keyboard, and resigns
  and becomes first responder on iOS), so it closed and opened again. Such a
  frame now runs a second egui pass without input, which drops that request
  (`phone_clipboard::keep_keyboard`); the tap still shows a keyboard that was
  put away.
- **Long text**: GameTextInput copies at most 64 KiB but reports the full
  length, and android-activity's reader for winit trusts that length. A Java
  `LengthFilter` keeps the keyboard's buffer under 21,845 UTF-16 units, which
  always fits, and the app's own reader takes only the copied bytes, cut to
  whole characters.
- **Keyboard type**: password fields and recovery words (`ui::Field::private`)
  get no suggestions and ask the keyboard not to learn
  (`IME_FLAG_NO_PERSONALIZED_LEARNING`). When a field stops wanting text the app
  hides the keyboard itself: winit's hide doesn't work once Android counts the
  keyboard as opened on purpose.
- **Typed secrets**: on Android, winit's "Unknown android_activity input event"
  warning, which prints the keyboard's text, is dropped before it is formatted;
  other warnings stay. GameTextInput's Java code logs typed text to logcat at
  debug level; R8 strips those calls (`deploy/android/proguard-rules.pro`),
  which needs a build that isn't debuggable. Rust copies are wiped: the buffer
  and the bridge's copy (`Zeroizing`, dropped as soon as focus moves), and the
  text events egui got, at the end of each frame. Leaving a secret field
  overwrites GameTextInput's native buffer (which never shrinks) before
  emptying it, in the same Java-thread job that then sets the next field's
  keyboard type, so a keyboard reconnecting in between never sees the secret
  with learning on. Some copies can't be wiped: android-activity's copy of the text
  for winit's (dropped) event, egui's undo history inside a field, Java strings
  (the keyboard's `Editable`), and whatever the keyboard app keeps.
- **Safe area**: system bars, display cutout and the keyboard come from
  GameActivity's window insets, read on the Java thread when the window
  changes size, when the keyboard opens or closes (for a moment while it
  slides), and every half second while it is open. The app draws inside
  what is left, so the focused field sits above the keyboard and is scrolled
  into view when the keyboard opens. The keyboard hides when focus leaves a
  field (egui and winit already did that).
- **Recovery words** show in two columns on phones.
- **Destroy**: the app runs eframe on its own winit loop and ends it once
  android-activity has handled `onDestroy`, waits up to 4 seconds for messages
  still being sent (a new activity's window waits too, and Android reports it as
  not responding after 5), then ends the process (see Android lifecycle). If
  messages were still on their way, it leaves a note (`unsent`, no message
  text); the next start says so in a chat and deletes the note once that showed. Drafts in the message box are lost, as
  on a desktop quit: no platform saves them yet.
- **Test build**: `deploy/android/build.sh` makes a release APK signed with the
  local debug key (`rusttalk-<version>-android-arm64-test.apk`), installed as
  `io.github.rusttalkcrew.rusttalk.test` ("RustTalk Test"), so it never
  blocks the release app: not
  debuggable, so a USB cable no longer opens the app's files, and R8 can strip
  logging. Published releases use the production signing key.

## State after phase 1

- `rusttalk-client` and `rusttalk-core` build for `aarch64-linux-android`,
  LiveKit and WebRTC included. `deploy/android/build.sh` made a debug APK.
- On a headless Android 15 emulator (x86_64 image running the arm64 build) the
  APK starts, draws the key storage screen, takes taps and opens the
  on-screen keyboard. (An earlier build that still offered the protected file
  also reached the welcome screen and the recovery words.)
- The APK was a debug build: debuggable, so anyone with the phone and a USB
  cable could read the app's files (phase 2 changed that).
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
| Voice and video (LiveKit, webrtc-sys) | Builds and links (prebuilt WebRTC for Android) | Prebuilt WebRTC for iOS; CI builds it | Android and iOS voice in phase 3; see Voice |
| Audio (cpal 0.18) | AAudio (`libaaudio.so`, Android 8.0+) | Core Audio | Android uses communication mode during calls; iOS needs its audio session |
| Key storage (keyring 4) | Android Keystore wraps the identity in private app storage | Data-protection Keychain, file with password as fallback | Both default to system key storage with password fallback |
| File picker (rfd) | No backend | No backend | Gated: the buttons that open it are off (`files::PICKER`) |
| Notifications (notify-rust) | Builds, no notification service | Builds, no notification service | Fail and are logged |
| Call camera | Camera2 session, rotated I420 frames | AVFoundation, oriented BGRA frames | Both feed the existing encrypted LiveKit camera track |
| Screen share, app and system audio | Not offered (`screen` already says unavailable) | Same | App audio gated to `Unsupported` |
| Updater | Not used | Not used | Gated: no update target, so no checks and no **Check now** |
| Global shortcuts (global-hotkey) | Builds, nothing to register | Same | Unused; Shortcuts tab still shows |
| Remote control | Not offered | Not offered | Unchanged (only Linux and macOS offer it) |
| Clipboard | Text through `ClipboardManager` on the Java thread | Text through `UIPasteboard` | Press and hold a field: Paste, Copy, Cut, Select all (`src/phone_clipboard.rs`); picture and file paste gated off |
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

After GameActivity loads the Rust library, `MainActivity.onCreate` passes the
application context to `initialize_android_context` through a native method.
The Java thread can find WebRTC's classes. A false return disables joining
voice and logs the failure. Gradle includes the jar from the same archive that
Cargo linked. R8 keeps the WebRTC classes and JNI Zero's native initializer.
The native library exports WebRTC's Java callbacks alongside GameActivity's.

Android requests `RECORD_AUDIO` at join. After permission is granted the app
sets `MODE_IN_COMMUNICATION` and takes audio focus before opening the
microphone, and releases both when the call ends. During a call `CallService`, a
microphone foreground service, keeps the microphone and the connection alive
with the app in the background or the screen locked. It shows an ongoing,
quiet "In a voice call" notification (Android resources in en, de, nl and ja,
following the phone's language) that opens the app. Joining asks for
notification permission on Android 13+; without it the service still runs and
only the notification is hidden. The service starts at join, while the app is
on screen, as Android 14+ requires; if Android refuses, the refusal is logged
and the call keeps working on screen. The camera stays foreground-only, so the
service declares no camera type. Background calls still need a device test.
Bluetooth routing is still open.

On iOS, joining asks for microphone access. With permission, RustTalk activates
a play-and-record voice chat session with speaker fallback and Bluetooth HFP.
cpal uses RemoteIO for capture and playback and resumes after interruptions when
iOS allows it. Its route errors feed the device reopen path. LiveKit publishes
our processed frames through a native audio source and uses a synthetic audio
device; it does not open a second hardware microphone. After leaving, the audio
worker releases the session when the leave sound finishes. The IPA declares
background audio for calls with the screen locked.

Turning on video asks for camera access. The front camera is the default;
The call panel shows a local preview from the same capture frames, mirrored only
for the front camera. Flip camera switches front/back without republishing the
encrypted track; Settings → Video uses the same selection. A failed switch restores
the previous selection. The rear-camera light is offered only when the active
device supports it, and displays the native capture state. Switching cameras,
turning off video, leaving the call or backgrounding the app releases the light.
Turning off video, leaving the call or backgrounding the app releases the camera.
After an interruption, turn video on again to resume sharing.

Android lifecycle invalidation is immediate: Activity locks only protect the camera
reference, and camera commands run on one process-wide worker. JNI callers wait at
most five seconds; native open and close waits are bounded. A device whose closure
is unconfirmed keeps its lease, so another capture cannot overlap it. Callback
handling stays alive to close delayed devices; retrying video after their close
acknowledgement can recover. Capture generation tags reject old frames even when
Rust has already started preparing another call. `make android-camera-test` drives
delayed callback, pause, timeout, rollback and resume cases with a fake backend; the
Android build runs it too. These tests and APK builds do not verify physical torch
or camera behavior. Device checks remain pending while Agent device access is off.

Joining a call on iOS is still untested.
On an iPhone, check the device log for permission, the active category/mode and
input route type, then five one-second capture counts. Verify a peer can hear
speech, including after an interruption, a headset switch and locking the screen.

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
  `libwebrtc.jar`). Its Java is `MainActivity`, a GameActivity that handles the
  keyboard's action key, microphone permission and call audio mode, the camera
  glue, and `CallService`, which keeps calls going in the background.

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
the Actions tab (`Mobile apps`), and keeps the test APK and IPA as artifacts.
Release tags build both with the Rust release profile. Only the protected
release job signs the Android package. Android version codes increase with
`major × 1,000,000 + minor × 1,000 + patch + 1` (0.2.0 is 2001).

## Phone layout

`ui::is_phone` is the one breakpoint: below 600 points the main layout is a
single column, in this order: server list and channels, then the open
channel, with the member list as a cover over it and Back to return. Picking a
channel, DM or page opens it.

Dialogs and the settings windows are full-screen sheets on phones (see
[Responsive UI](development.md#responsive-ui)); Settings and Server settings
list their pages first, then show the picked page with Back. Touch builds
make buttons, rows and menu rows at least 44 points tall and widen every
widget's tap area by 12 points.

Android's Back button and gesture (`src/back.rs`) close what is on top, one
thing a press: a menu or popup, then a dialog or settings window (a settings
page goes back to its list first), then the member list over a chat, then the
open pane goes back to the list, like the Back bar. `MainActivity` turns on its
`OnBackPressedCallback` only while there is something to close; at the list,
Android handles Back and moves the app to the background (with the predictive
back animation, `enableOnBackInvokedCallback`). Without predictive back,
GameActivity would give the Back key to native code, so `MainActivity` sends it
to the app instead, which moves itself to the background at the list.

Still to do:

- **Call panel and voice channel:** controls in a bottom bar, the participant
  list as a cover like the member list.

## Later phases

Rough effort for one person who knows the code base.

| Phase | What | Effort |
| --- | --- | --- |
| 2 | Usable chat on Android. Done: typed text, safe areas and the keyboard, `onDestroy`, recovery words, Android Keystore. Left: phone temp folder (`temp_dir()` is not writable on Android), file picker (Storage Access Framework), local notifications, hide Updates and Shortcuts on phones, `rusttalk://` links, the layout list above | 3 to 4 weeks |
| 3 | Foreground voice on Android and iOS; iOS requests microphone access and supports background audio. Left: iPhone device verification, Android Bluetooth routing, background-call device test | 2 weeks |
| 4 | iOS on a device. Done: Keychain, safe area and taps. Left: notifications (reuse `notify/macos.rs`), document picker, status bar colours, background rules | 1 to 2 weeks |
| 5 | Foreground call camera and front/back selection. Left: verify capture and interruptions on a real iPhone | Device verification |
| 6 | Release downloads and signing are in place. Left: store listings and store updates | 1 week plus store review |
| Later | Screen share (MediaProjection; ReplayKit broadcast extension), push notifications | 2 to 4 weeks each |

## TLS trust

On Android and iOS, HTTPS and WebSockets (including LiveKit signaling) use
Mozilla's public roots bundled with the app. The locked native loader looks
for OpenSSL files on both platforms. The bundle updates with the app and
needs no runtime download. Device-installed CAs and OS revocation policy
do not apply. An explicitly pinned server still requires its exact leaf
certificate.

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
- **Android permissions:** `INTERNET`, `MODIFY_AUDIO_SETTINGS` for call routing,
  `RECORD_AUDIO` when joining voice, and `CAMERA` when turning on video.
  Cameras are optional. `FOREGROUND_SERVICE` and `FOREGROUND_SERVICE_MICROPHONE`
  keep calls going in the background; `POST_NOTIFICATIONS` (asked at join on
  Android 13+) shows the call notification. Planned: `BLUETOOTH_CONNECT` for
  headsets. Never location, contacts or phone state.
- **iOS:** the bundle states why it uses the microphone and camera; iOS asks
  the person on first use.
- **Storage:** settings and the key file live in the app's private storage.
  Android: `allowBackup="false"` and data extraction rules that exclude
  everything, so nothing goes to Google's cloud or to a new phone. iOS: the
  folder is Application Support, marked excluded from backups
  (`NSURLIsExcludedFromBackupKey`) before the key storage opens, so nothing
  goes to iCloud or a computer.
- **Keys** (`keystore::RULES`, see [Crypto](crypto.md#local-identity)):
  Android wraps the identity with a non-exportable Android Keystore key. A new
  account uses a password if the encryption and decryption probe fails. Existing
  password files still require their password until moved in Settings > Privacy >
  Key storage. Uninstall removes the wrapping key. Restoring a wrapped file alone
  leads to recovery with words. iOS
  keeps keys in the data-protection Keychain with `when-unlocked-this-device-only`
  access (never in iCloud Keychain; a backup restores it only to the same
  iPhone), through a small
  backend of our own on keyring's `apple-native-keyring-store` (keyring's `v1`
  API has no iOS store), and falls back to "File with password" when the
  Keychain can't be written (see iPhone above). A key file found without a password, or a
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
