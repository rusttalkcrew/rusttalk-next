# Changelog

<!--
One source for the in-app "What's new" page and the website roadmap's Shipped timeline.
Format (parsed by crates/client/src/whats_new.rs, keep it strict):
  - `## Unreleased` first, then `## vX.Y.Z - YYYY-MM-DD`, newest first.
  - Under each heading only `- ` bullets, one line each, short and user-facing.
Every user-visible change adds one line under Unreleased in the same PR. A release moves
Unreleased under the new version and date.
-->

## Unreleased

- Direct messages and mentions play a short chime from RustTalk itself, also when the system keeps notifications quiet; turn it off in Call sounds.
- While you are in a voice call, RustTalk keeps you Online even when another app has the focus.
- Reopen the last chat picture instantly, even if the connection drops after it loads.
- Unavailable servers show an exclamation mark in the server sidebar until they reconnect.

## v0.2.1 - 2026-11-23

- Incoming custom emoji now appear as soon as their pictures finish loading, without waiting for another chat message.
- Backgrounding Android during a camera switch no longer waits for the camera or reopens it after pause.
- Retry a failed forum post with its title, body and poll. Replies stay visible when a poll and its post arrive separately.
- Forum channels: searchable posts with folded replies, polls, up and down votes and sorting.
- Close image previews by clicking or tapping the dimmed space around them.
- Oversized stored uploads show their size and the app limit beside chat or shared files, including voice channels.
- Screen shares use the sharer’s participant card, show distinct viewer counts, and preview your screen while the window is focused.
- Mobile calls show your camera preview with front/back switching and a supported rear-camera light.
- Shared app and screen audio stays silent until you choose Listen; Stop listening ends it immediately.
- Kicking a member now asks for confirmation by name and keeps the original member and server selected.
- Ask someone to join your voice channel: it rings, and they can join, decline, come later or mute you for 15 minutes
- Servers can turn on Events: a calendar above the channels to suggest, approve and add events, in real life or in a channel
- Say whether you are going to an event, see who is still missing, and remind them
- Event reminders reach unanswered authors and respect “Only when RustTalk isn’t focused” without repeating later
- Channel names show the pointing-hand cursor when hovered, including for channel managers.
- Desktop: call a phone number into your voice channel with your own SIP account, contacts and a keypad. Phone calls use unencrypted SIP/RTP; the provider and network path can hear them.
- Listen to phone audio separately from sharing your microphone with the phone. The caller shares their own microphone; other members must consent; stopping sharing pauses the channel microphone until exclusion is confirmed or the call ends.
- Phone calls end on reconnect or lost permission; hold and one-way audio are not supported.

## v0.2.0 - 2026-11-23

- Reopen RustTalk offline to see previously loaded server icons, avatars and recent chat messages.
- Chat fills empty voice channels. Use Join voice to start a call.
- Choose one channel for joins and leaves, or turn them off. Device changes appear only in the audit log.
- Phones can watch shared terminals. Android can also share its own terminal when enabled in Settings > Advanced.
- Android can keep your account keys on this phone without asking for a password at every start.
- Find servers and join public servers on iPhone and Android.
- Phones and small windows: Settings, dialogs and every screen now fit the screen, with bigger buttons to tap
- New look for the first-start screens, with a Back button, Copy for your recovery words and Paste to fill them in
- Phones: tapping a text field that is already open no longer closes and reopens the keyboard
- iPhone: the keyboard no longer covers what you type, Return works, a tap outside closes the keyboard, and the edges match the theme
- iPhone calls request microphone access and support background audio.
- Android asks for microphone access when you join a voice call.
- Phone calls have front and back camera controls.
- Download the signed Android APK or sign the experimental iOS IPA with your own Apple account.
- Android: typing works, so you can sign up, sign in, join a server and chat
- Enter now confirms the name and unlock screens
- On touch screens, hold to open a menu or tap a message or call tile to show its controls.
- Phones: press and hold a text field to paste, copy, cut or select all, using the phone's clipboard
- iPhone: taps land where you touch, and the app stays clear of the notch and home bar
- iPhone: your keys are kept in the Keychain, so there's no password to type

## v0.1.11 - 2026-11-22

- Your microphone works again when a moderator unmutes you, without rejoining the call
- A moderator mute also stops shared app and stream sound; share it again after unmute
- Edit channel is one dialog with General and Permissions tabs
- Update checks are on by default and the update address is filled in
- Minimizing everyone on the call stage no longer shows "Connecting…"

## v0.1.10 - 2026-11-22

- Update required: older apps must update (protocol version 3)
- Removing a device changes chat and call keys, so it can't read new messages
- Every channel has a Files list: share files and download them later
- Pages can show pictures, and links on a page get preview cards
- Pictures on a page stay while the page uses them
- Link preview text no longer runs underneath the Open link button

## v0.1.9 - 2026-11-22

- Remove another device from Settings using your recovery words
- A removed device is refused and stops getting keys on every server
- Let viewers control a shared screen on macOS
- Share app audio and all system audio on macOS
- Clicking a macOS notification opens the chat
- Install guides for Windows, Linux, macOS and the Steam Deck

## v0.1.8 - 2026-11-22

- Type in a shared terminal when the sharer allows it
- Page channels: one Markdown page that only editors can change
- Emoji and sticker packs per server, usable on every server, import from a .zip
- Update banner: update and restart in one click, checks every 15 minutes
- More options menu (…) in the call panel
- What's new page in Settings, shown once after the app updates
- Polls in chat, end-to-end encrypted
- Share a terminal in a voice channel (read-only)
- One name for every server, with per-server names, and a profile bio
- Show who a reply quotes before the original loads
- Restore missed DM history on shared servers
- Keep keys in a file when there is no system keychain (Steam Deck Game Mode)

## v0.1.7 - 2026-11-22

- Audio mixer per person: voice, shared apps and stream sound
- Global shortcuts for mute, deafen, push-to-talk and sharing
- Right-click menu on screen shares, with FPS and stats overlays
- Previews for YouTube, Spotify, GitHub, Wikipedia and Reddit links, and for other websites (made by the sender)
- Server settings Members: a searchable, sortable table with a role filter
- Animated server emoji and stickers always play, with an on-hover setting
- Signing out now clears account files and preferences
- Same click behaviour for people everywhere: a profile card and one shared menu
- macOS builds are published in releases (not signed by Apple yet)
- The default server directory is now index.rusttalk.com

## v0.1.6 - 2026-11-21

- Markdown and highlighted code in chat messages
- Voice moderation: mute for everyone, remove from and move between voice channels
- Hide from member lists while you are offline
- Share all system audio, and give screen shares their own sound
- A redesigned message bar, with the emoji and sticker pickers merged
- Invite links and Steam store links show as cards in chat
- Attach files dropped from a file manager on Wayland
- Let the sharer preview their own screen share
- Open voice channels with a click and join with a button
- Remote control for Hyprland monitor shares
- Quieter calls: fans stay off in quiet calls and AI noise removal is adjustable

## v0.1.5 - 2026-11-21

- Animated GIF and WebP profile pictures and server icons, played on hover
- Voice channel audio bitrate setting
- Screen share and camera at a real 60 fps, with lower latency
- Sounds when you mute, unmute, deafen and undeafen yourself
- Remote control and video keep working while the server connection is down
- Stopping a screen share you watch now sticks
- Someone who switches voice channels shows only in the new one
- Fixed servers sometimes answering with a database_busy error when one write ran long

## v0.1.4 - 2026-11-21

- Poke someone with a short note
- Show and play GIFs and WebP inline in chat
- Paste pictures and copied files into the chat, with a clearer drop zone
- Local sounds for voice and stream events
- Suggestions for noise suppression when the microphone is noisy or too loud
- A redesigned roles editor
- Edit file and sticker limits in server settings
- Servers come back after restoring from the recovery words
- The voice and user panel stretches under the server rail

## v0.1.3 - 2026-11-21

- Text chat in voice channels
- Direct messages over any server you share, and friends, with an option to allow DMs from friends only
- Encrypted mentions for people, roles and everyone
- Unread marks, desktop notifications and per-channel notification settings
- Let a viewer control a shared screen, with the sharer's permission
- The call stage with focus, bubbles, presentation mode and split layouts
- Switch what you share without stopping the screen share
- Colour emoji, shortcode completion and a chat picker
- Custom server emoji and stickers
- The app in English, German, Dutch or Japanese, with a language setting
- One-click app update with a confirmation dialog
- Optional AI noise removal for the microphone, and automatic voice sensitivity

## v0.1.2 - 2026-11-21

- One design language: new themes, accent colours and the Inter font
- Settings and Server settings with a sidebar and pages that scroll by section
- Reorder channels and categories by dragging, and rename or delete them
- Resizable sidebars and a collapsible member list
- Crop and resize when picking a profile picture or server icon
- A cleaner voice panel and user bar
- The official servers are offered on the join page
- Screen share choices rebuilt, with a 1440p option

## v0.1.1 - 2026-11-20

- Join voice channels on servers reached by a domain name

## v0.1.0 - 2026-11-20

- Text channels and direct messages with replies and edits
- Voice, camera and screen or window sharing with app audio
- Several servers at once
- Roles, kick, ban and an audit log
- Invite links
- Presence: online, away or invisible
- Avatars and server icons
- Encrypted stored files, and live file transfer
- Settings and server list synced between your devices
- Optional public server directory
- Signed self-updates
- Windows and Linux, x64 and ARM64
