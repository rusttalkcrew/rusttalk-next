# Install RustTalk on the Steam Deck

You install RustTalk in Desktop Mode, then add it to Steam so it also starts
from Game Mode. To switch to Desktop Mode, press the **Steam** button and
choose **Power → Switch to Desktop**.

## Install (Desktop Mode)

1. In a web browser, download `RustTalk-linux-x86_64.AppImage` from the
   [releases page](https://github.com/rusttalkcrew/rusttalk-next/releases/latest).
   It lands in **Downloads**.
2. In the **Dolphin** file manager, right-click the file, choose
   **Properties → Permissions** and tick **Is executable**.
3. Double-click the file to start RustTalk, then sign in or create your
   account.

## Move your keys to a file first

Game Mode has no keychain, but Desktop Mode does, and RustTalk keeps your keys
there by default. If you skip this step, Game Mode says "Your keys are in the
desktop keychain" and can't open them. While still in Desktop Mode:

1. Open **Settings → Privacy → Key storage**.
2. Pick **Protected file** (recommended: a file only you can open, nothing to
   type) or **File with password** (safest: you type the password at every
   start).
3. Click **Move keys**. RustTalk says "Done. Your keys are in their new place."

Making a new account in Game Mode instead? RustTalk asks the same question on
first start ("Where should RustTalk keep your keys?"). You can change it later
on the same Settings page.

Forgot the password? Choose **Use recovery words** on the unlock screen and
sign in again.

## Add it to Game Mode

1. In Desktop Mode, open **Steam**.
2. Choose **Games → Add a Non-Steam Game to My Library**.
3. Click **Browse…**, set the file type to all files, pick the AppImage and
   click **Add Selected Programs**.
4. Switch back to Game Mode. RustTalk is in your **Library**, under
   **Non-Steam**.

Keep the AppImage where you put it. If you move it, add it to Steam again.

## Using the controller

- The right trackpad works as a mouse. If the pointer doesn't move, open
  **Controller Settings** for RustTalk and pick a layout with a mouse, such as
  **Gamepad with Mouse Trackpad**.
- Open the on-screen keyboard with **Steam + X**. The key storage screens use
  big buttons and accept Enter.

## What may not work in Game Mode

- There is no keychain, which is why RustTalk keeps your keys in a file.
- Screen sharing and global shortcuts depend on the desktop and haven't been
  tested in Game Mode. If they don't work, use Desktop Mode.

## Updates

When a bar at the top says "RustTalk vX.Y.Z is ready", click **Update and
restart**. The AppImage stays where it is, so the Steam entry keeps working.
