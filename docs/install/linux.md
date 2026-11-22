# Install RustTalk on Linux

RustTalk comes as an AppImage: one file that runs without installing.

## What you need

- A 64-bit Linux desktop, Intel/AMD (x86_64) or ARM (aarch64). The AppImage is
  built on Ubuntu 24.04, so use that or something newer.
- PipeWire for sound and `xdg-desktop-portal` for file dialogs and screen
  sharing. Most current desktops have both.
- FUSE, so the AppImage can start. If it won't start, install your
  distribution's `fuse3` package.
- A keychain such as GNOME Keyring or KWallet to keep your keys. Without one,
  RustTalk asks where else to keep them on first start.

## Install

1. Download `RustTalk-linux-x86_64.AppImage` (or `RustTalk-linux-aarch64.AppImage`)
   from the [releases page](https://github.com/rusttalkcrew/rusttalk-next/releases/latest).
   Put it in a folder you own, for example `~/Applications`.
2. Make it runnable. Either right-click the file in your file manager and
   open **Properties → Permissions**, then tick "Allow executing file as
   program" (KDE: **Is executable**), or run:

   ```sh
   chmod +x ~/Applications/RustTalk-linux-x86_64.AppImage
   ```

3. Double-click it, or run it from a terminal.

## Add it to the app menu

RustTalk doesn't add itself to the app menu. An AppImage manager does it for
you, for example [AppManager](https://github.com/kem-a/AppManager): open the
AppImage with it and follow its steps.

## Updates

RustTalk updates itself. When a bar at the top says "RustTalk vX.Y.Z is
ready", click **Update and restart**. It replaces the AppImage file it was
started from and opens again. This needs:

- the AppImage in a folder you can write to;
- starting it as an AppImage, not unpacked and not with
  `--appimage-extract-and-run` (those still run, but can't update themselves).

If a manager moved the file, RustTalk updates the copy it is running from.

## Wayland and X11

- Dragging files into the window works on Wayland.
- Global shortcuts (mute, deafen, push-to-talk, sharing) are set in
  **Settings → Shortcuts**. On Wayland the desktop's global shortcuts portal
  handles them (KDE, GNOME 48 or later, Hyprland) and may ask you to confirm
  the keys.
- On a desktop without that portal, bind a key in your desktop's own settings
  to a RustTalk command, using the full path of your AppImage:

  ```sh
  ~/Applications/RustTalk-linux-x86_64.AppImage --toggle-mute
  ```

  The others are `--toggle-deafen`, `--share-window` and `--stop-sharing`.
  For push-to-talk, bind `--push-to-talk` on key down and
  `--push-to-talk-release` on key up. RustTalk must already be running.

## Uninstall

Delete the AppImage. Your settings and keys stay. To clear them too, use
**Sign out of this device** in RustTalk first, but only if you still have your
recovery words.
