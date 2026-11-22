# Install RustTalk on macOS

Needs macOS 14.2 or later. One download works on Apple Silicon and Intel Macs.

## Install

1. Download `RustTalk-macos-universal.dmg` from the
   [releases page](https://github.com/rusttalkcrew/rusttalk-next/releases/latest).
2. Open the file and drag **RustTalk** onto **Applications**.
3. Eject the disk image.

Always open RustTalk from **Applications**. Opened from Downloads or from the
disk image, it can't update itself.

## Open it the first time

RustTalk isn't signed by Apple yet, so macOS stops the first start. Follow the
steps for your macOS version.

**macOS 14 (Sonoma):**

1. In Applications, right-click **RustTalk** and choose **Open**.
2. Click **Open** in the question that appears.

**macOS 15 (Sequoia) or later:**

1. Open RustTalk once. macOS says it can't check it. Click **Done**.
2. Open **System Settings → Privacy & Security** and scroll down.
3. Next to the message about RustTalk, click **Open Anyway** and confirm.

You only do this once. Prefer Terminal? This does the same:

```sh
xattr -dr com.apple.quarantine /Applications/RustTalk.app
```

## Permissions

macOS asks when RustTalk first needs something. Click **Allow**:

- **Microphone**, so people can hear you.
- **Camera**, when you turn on video.
- **Screen & System Audio Recording**, when you share your screen or a window.
  If you allow it later, do it in **System Settings → Privacy & Security →
  Screen & System Audio Recording**, then reopen RustTalk.

Sharing the sound of one app isn't available on macOS yet.

## Updates

When a bar at the top says "RustTalk vX.Y.Z is ready", click **Update and
restart**. RustTalk replaces itself in Applications and opens again. To check
by hand, use **Settings → Updates → Check now**.

Because each build is signed on its own, not by Apple, macOS may ask again for
the microphone, camera, screen recording and keychain after an update. Allow
them again. Your account and settings stay.

## Uninstall

Drag **RustTalk** from Applications to the Bin. To clear your keys and
settings too, use **Sign out of this device** in RustTalk first, but only if
you still have your recovery words.
