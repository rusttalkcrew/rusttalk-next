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

- Remove another device from Settings using your recovery words.

- Once a device learns about a revoked device, it refuses its messages and stops sharing keys with it across servers

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
