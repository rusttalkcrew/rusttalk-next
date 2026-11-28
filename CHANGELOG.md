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

- Server settings > Statistics shows owners and admins who is connected, sign-ins, messages, traffic, storage, uptime and memory, refreshed every 2 seconds. Only counts and sizes, no content.
- Server statistics also show who is in voice, audio and video streams, and traffic split into voice and video and the rest.

## v0.12.0 - 2026-11-28

- On a phone held sideways, the voice panel in the sidebar keeps to one row of buttons and the server list scrolls, so the channel list keeps its room.
- Camera pictures look smooth instead of grainy in small tiles: they are scaled down by averaging, like the system's own camera preview.
- Show the whole call full screen at any time with the new button at the top right of the call.
- The floating camera self view on phones no longer covers the voice panel or your name in the sidebar.
- Right-click the mic, sound, camera or screen share button in a call to pick a device, start sharing or set screen share quality.
- Report a message from its menu, with a reason and an optional note. Moderators with the new "Review reports" permission (for DMs, only the server owner) see it under Server settings → Reports, checked against the sender's signature, and can delete the message, time out, kick, ban or dismiss. The server can't read reports, and nothing besides the reported message and its files is shared.

## v0.11.0 - 2026-11-27

- Zoom pictures in the viewer with the scroll wheel.
- Adding many files to a channel's shared files at once no longer fails when the server asks you to slow down: the app waits a moment and carries on.
- Server owners can remove illegal content with "Remove content" on a message or by pasting its ID in Server settings → Takedowns: the message, its edits and its files are deleted from the server, the account can be banned in the same step, and a record without the content is kept.
- Channel managers can mark a channel NSFW. It shows an NSFW badge, and the first time you open it on a device you are asked whether to continue; your answer is remembered on that device only.
- Sensitive pictures and videos show blurred until you tap them. Tick "Mark as sensitive" when sending, blur or unblur any picture from its menu, and choose in Settings → Privacy whether marked pictures blur and whether the app spots likely nude pictures on your device.
- Report a server in Find servers, and see on Public listing when the directory removed your server and why.

## v0.10.0 - 2026-11-27

- Names can no longer contain links or invites. Someone whose name has a link shows as "Member" with a short code, so a pasted invite is never shown to everyone.
- On a computer, opening a channel or DM puts the cursor in the message box so you can type straight away.
- Settings → Appearance has a Time format choice: Automatic (follows the app language), 12-hour (2:58 PM) or 24-hour (14:58). It applies to every time in the app and follows you to your other devices. Event and focus mode time fields also accept times like 8 PM.
- Block someone from their profile card, right-click menu or the Friends list: their messages in servers fold into a "Blocked message" line you can open, their voice is muted for you, and their DMs, pokes and friend requests don't reach you. Manage the list in Settings → Privacy; it follows you to your other devices and servers never see it.
- DMs from people who aren't your friends now wait under Message requests, quietly, until you accept, delete or block them. In Privacy settings you choose who can message you: everyone, server members (as requests) or friends only. Servers limit how many new DMs someone can start per hour.
- Channel managers can turn on slow mode for a busy channel, from 5 seconds to 6 hours between each member's messages. The message box counts down until you can send again, and moderators can skip the wait.
- Servers now slow down members who post, react, upload or invite much faster than a person could, and owners pick Relaxed, Standard or Strict in Server settings; a message sent too fast waits a moment and goes out by itself.
- Server owners can turn on new member rules: for 10 minutes, an hour, a day or a length you choose after joining, members with only the default role can't upload files, start DMs, mention @everyone or roles, or create invites. New members see a note saying when that unlocks.
- Moderators can time out a member for 60 seconds up to a week, with an optional reason: until it ends they can read but not send messages, react, create invites or join and talk in voice, and they see a banner saying until when. Moderators can end it early.
- Raid tools: lock your server down so nobody new can join, automatically when many people join at once, and remove a banned person's messages from the last hour, day or week.
- On phones, a small self view of your camera stays on screen while your own tile is out of sight, in full screen or outside the call view. Drag it to a corner or minimize it.

## v0.9.1 - 2026-11-26

- On Android, HEIC photos now work as attachments, profile pictures, server icons, emoji, stickers and page pictures; they are turned into JPEG on your phone.
- On phones the sticker and emoji panel and the keyboard no longer show together: tapping the message box swaps the panel for the keyboard, and tapping the chat closes the panel. On a computer, clicking the message box keeps the picker open. A long-pressed message stays highlighted while its menu is open.

## v0.9.0 - 2026-11-26

- Send up to 10 files in one message, so a whole photo collage fits in one album (on updated servers; others still take 4)
- Clicking the line above a reply now jumps to the message it answers and briefly highlights it, loading older messages when needed; the line itself is calmer, with the person's picture and a short preview.
- On phones, tapping a message no longer shows the desktop toolbar; press and hold a message to open its menu with react, reply, copy and delete.
- Large phone photos sent without a preview now show in albums and the gallery too, and right-clicking (or long-pressing) a picture in the viewer lets you copy or save it.
- The member list now shows people who are online first in each group, then Do not disturb, Away and offline, by name within each.
- The sticker and emoji picker has a row of tabs (recent stickers and one per pack) over a grid of four large stickers, and animated stickers and emoji play all the time. On phones it opens in place of the keyboard, and a tapped sticker shows large above the message box first: tap it again or press send to send it.

## v0.8.1 - 2026-11-26

- Several pictures or videos in one message now show together as one album, and the picture viewer steps through them with the arrow keys, on-screen buttons or a swipe, showing which one you are on.
- Large phone photos (up to 48 MP and wide panoramas) now show as pictures with a preview, the right way up, instead of as a file; files waiting to be sent show as equal tiles you can scroll through.

## v0.8.0 - 2026-11-26

- Reconnecting after a network drop is quicker on busy servers: the app keeps the keys and devices it already checked instead of checking them all again.
- Android: attach files and photos, change your profile picture or server icon, add emoji and stickers and import emoji packs, using the phone's own file and photo picker.
- On iPhone you can now upload files and photos: attachments, profile picture, server icon, emoji, stickers and page pictures.
- iPhone: voice calls show as calls on the lock screen, in the Dynamic Island and in Control Center, with mute and hang up; they stay out of the Phone app's Recents.
- On Android, the voice call notification has Mute and Hang up buttons that work with the app in the background, and on Android 12 and newer it shows as a real ongoing call in the status bar; tapping it opens the call.
- Reconnecting after a network drop is faster when you have many chats and DMs open: their newest messages now reload side by side instead of one by one.
- Animated profile pictures and emoji use less memory, and many of them playing at once no longer slow the app down.
- Draw a diagram together in a call: start one from the call's … menu, everyone sees it on the stage, and others can ask to edit. On desktop, save it as a draw.io file or start from one.
- A Gallery button in channels and DMs shows every picture and video shared there as a grid, with the viewer stepping through them and a jump back to the message.
- Security: a direct message's key is now only accepted and passed on when it comes from one of the two people in it, also when the app first learns about that direct message.

## v0.7.0 - 2026-11-25

- Voice channels can now be set to audio quality up to 320 kbps, for listening to music together.
- People who manage channels see empty categories again, so a new or emptied category no longer disappears; members still don't see them.
- The call connection panel has a cleaner look, with easier-to-read numbers and graphs, and shows the voice channel's audio quality.
- Server settings now have separate Emoji and Stickers pages: pick or drag in several pictures at once, see them large with their names before uploading, and click any emoji or sticker to rename, move or delete it.
- The server menu's "Change name on this server" is now "Change my nickname", so it isn't mistaken for renaming the server.
- The sound bars next to a shared app now move clearly even when it plays quietly.
- Someone sharing their camera and screen at once no longer loses their camera: it shows as a small picture on their screen that you can drag to any corner, pop out into its own tile or shrink to a small button.
- Popping a camera out of a screen in full screen now leaves full screen and shows the camera as its own tile, instead of hiding it.

## v0.6.0 - 2026-11-25

- Your status no longer gets stuck on Away for others (while your own bar says Online) when you change it just as the app reconnects.
- Using RustTalk on several devices: an idle phone no longer sets you Away while you use your computer. You show as Away only when all your devices are idle, and a status you pick on one device applies on all of them.
- New status: Do not disturb. Others see a red dot with a bar, and none of your devices show notifications or play message sounds while it is on (unread counts still update).
- Apps someone shares sound from show under them in the voice channel: click (or tap) one to listen and set its volume, and small bars move while it plays.
- You no longer turn "Away" while you use other apps: on computers, any keyboard or mouse use counts, not just input in RustTalk.
- Server owners can set file limits (storage, how long files are kept, stickers, and file size up to 32 MB) as high as they like in Server settings, unless the server's host set a cap; the settings now say what that cap is and how the host can raise it.
- Full screen streams keep the call's buttons and the stream's own menus, shown when you move the pointer (tap on phones) and hidden when idle; Escape or the hover button leaves full screen.
- A voice channel's member limit can be removed again by clearing the field, and the "2/8" count in the channel list now has a people icon and says "Member limit: 8" on hover (or a tap on phones).
- Right-clicking (or long-pressing) a message's words or links now opens the message menu, like the rest of the message.
- Channel managers no longer see empty categories in the channel list; "Show empty categories" in the server menu brings them back for editing.
- Admins can preview the server as one or more of their own roles ("View server as this role" in Roles): channels, categories and buttons show as those roles would see them, and nothing can be changed while previewing.
- The emoji or sticker name you typed in Server settings now stays when you cancel picking a picture or the upload fails.
- Saving roles is faster, and if part of it fails, the changes that didn't go through stay waiting for Save.
- Server owners can choose how many messages each channel keeps, or keep them all, in Server settings > Message history (10,000 by default).
- Press Ctrl+K (Cmd+K on Mac), or the search button on phones, to jump to a person, a channel on any of your servers, or a settings page.
- The audit log now records server setting, file limit and public listing changes, and shows which roles and channel permissions changed.
- In chat, the name lines up with the top of the avatar and the message sits right under it, also when it starts with a mention; a reply's quote now sits above the name.
- Reactions light up when you point at them, and "who reacted" shows the real emoji picture.
- Animated profile pictures now play while the person is speaking in a call, and stop on the first frame when they go quiet.

## v0.5.0 - 2026-11-25

- Android test builds install as a separate "RustTalk Test" app, so they no longer block installing the release app.
- On phones, mute, sound, camera and leave buttons now sit at the bottom of the call view, and the call opens without the chat (tap the chat button to show it).
- On phones, your own camera shows only in the call view, and while it is on, tapping the camera button opens a menu to flip it, switch the light or turn it off.
- "Request control" is greyed out, with the reason, when the sharer turned control requests off or their computer can't be controlled, and a refusal now shows on the shared screen.
- React to any message with an emoji, end-to-end encrypted: click a reaction to add or remove yours, hover to see who reacted.
- Right-click or long-press a picture, GIF or sticker in chat to copy it, save it or open it in the viewer; messages can also copy their ID.
- Android calls keep working when you switch apps or lock the screen, with an "In a voice call" notification that takes you back.
- On Android, Back now closes menus, dialogs and settings pages and returns to the channel list before leaving the app.
- Invite links stop working when the person who made them leaves, is kicked or banned, or loses the right to invite.
- Pressing Escape in a settings field or clicking outside Settings no longer throws away what you typed: unsaved changes ask before they are discarded.
- In a DM, only the person who wrote a message or sent a file can delete it, also when the other person is a moderator.
- Role managers can give someone a new role without losing roles above their reach, and can no longer take those roles away.
- On phones, swipe in from the left edge to open the server and channel list, and swipe it away or tap beside it to close it.
- On phones, screenshots and the recent apps view no longer show your recovery words.
- Moderators can no longer lift a ban placed by the owner or by someone with the same or a higher role.
- Android: hiding the keyboard with Back no longer keeps the app redrawing in the background, saving battery.
- On phones, a server that needs a newer app now offers "Download the new version", which opens the download page, instead of the desktop updater
- Deleting a single custom emoji or sticker in server settings now asks first.
- Server settings > Emoji & stickers fits on a phone: name fields make room for their buttons, warnings wrap, and each emoji's Rename, Move to and Delete sit under "…".
- Phones: save a file from a message to Downloads on Android, or share it from iOS. The "Open" button that did nothing on phones is gone.
- Server settings open faster: the audit log loads only on its own page, newest 50 first, with Show older for more
- The Members list in server settings is readable on phones: names get the row, roles sit under them and the presence dot moves onto the picture.
- Dialogs no longer get cut off on short windows: they scroll, phones held sideways show them full screen, and the close button is easier to tap on tablets.
- Server settings > Joining: Save lights up only after a change, and the page shows fresh values when you come back. Phones no longer list Shortcuts and call themselves "This device".
- Network route problems (proxy, Tor or WireGuard) now show short, translated messages like "Couldn't reach the proxy", with the technical text under Details.
- Security: people who manage channels can only see, edit, move, delete or change access to channels they can read themselves
- Security: slow or stalled uploads can no longer make a server refuse everyone else's changes.
- Security: device removals from other members can no longer stop the app from syncing keys, and you can always remove your own devices.

## v0.4.0 - 2026-11-24

- Changing your name, picture or bio no longer pauses chat and voice on every server while your devices sync.
- Servers stay responsive when someone joins: sharing chat keys no longer slows down everyone else's connection.
- After sleep or a network hiccup, servers reconnect within about 30 seconds instead of silently missing new messages.
- Channels with edited messages load faster.
- Servers come online faster after connecting or reconnecting, without waiting for your devices to finish syncing.
- Reconnecting to many servers at once is faster: syncing with your other devices no longer holds each server up.
- Joining a voice channel is faster, and chat, Leave and switching channels keep working while it connects.
- Pictures, link previews and big files load and upload in the background, so chat, status and voice keep up meanwhile.
- Joining or leaving a voice channel updates everyone's channel list faster, with less load on the server.
- New chat messages show up faster, with less work for the server.
- Servers keep up better when many people are online, so fewer full reloads after busy moments.
- Connecting to a server is a little faster: two repeated requests are gone.
- After a server restart or a Wi-Fi hiccup, RustTalk reconnects within about 10 seconds instead of up to 30.
- New messages show up faster, especially on slow disks and phones.
- Many devices behind one home router can sign in to a server at once, and reconnects no longer hit the sign-in limit.
- Changing your name, picture or bio, or editing channels, emoji and events, no longer makes everyone's app reload the whole server.
- Loading many things from a server at once is faster: requests now share one HTTP/2 connection.
- Avatars, server icons and custom emoji are kept in an encrypted cache, so they show right away after a restart instead of downloading again.
- Servers with animated avatars, icons or emoji open faster and use less memory: their animations load the first time they play.
- Servers come online faster after starting or reconnecting, and clicks, messages and call joins no longer wait for pictures and unread marks to load.
- Your chat messages show up right away while they are sent; one that fails stays in the chat with Try again.
- Others see you come online sooner when the server is busy.
- During a voice call, a dropped server connection is now noticed and reconnected within about 30 seconds, so chat and status keep arriving.
- Picture-heavy channels no longer download the same thumbnails over and over; pictures on screen stay loaded and only load once they scroll into view.
- Pictures, files and messages still uploading or downloading when the connection drops or the network changes are tried again once it is back, instead of failing as offline.
- Reading a busy channel no longer stutters: read marks are saved every few seconds in the background instead of after every new message.
- Voice calls use less CPU and battery: the app no longer redraws ten times a second for the whole call, only when someone starts or stops speaking.
- Big servers answer key and membership checks faster: the server now keeps its database statistics up to date.
- Adding big files to a channel's shared files no longer freezes that server: new messages keep arriving and calls keep going while they upload.
- Member lists on big servers scroll smoothly.
- Long chats scroll smoothly: only the messages near what you are reading are drawn.

## v0.3.0 - 2026-11-24

- Put a voice channel into focus mode for up to 24 hours: it gets a Focus badge, a countdown banner and a short notice before anyone joins.
- Channels only some roles can see show a lock, and their permissions are easier to set.
- Voice channels can have a member limit; the channel list shows how full they are, like 3/5.
- The stream badge shows a compact viewer count and leaves the video stats visible.
- Choose Direct, SOCKS5, app-only WireGuard, or Tor during onboarding and in Settings → Network on desktop and mobile, with per-device settings and secure credential storage.
- Open a profile from a message author’s name or picture.
- A temporary error syncing your account picture no longer disconnects the server.
- The bottom-left account picture now lines up with your name and status.
- Your account picture now follows you to new servers and repairs missing pictures after reconnecting.
- Rounded-square avatars across the app, a speaking highlight on your bottom-left avatar during voice calls, and subtle hover feedback on sidebar server icons.
- Incoming live file offers now have a single Download button.
- Screen shares show simpler controls, preview automatically while focused, and start with system audio selected where supported.
- Direct messages and mentions play a short chime from RustTalk itself, also when the system keeps notifications quiet; turn it off in Call sounds.
- In the direct message list, the red badge no longer covers the server names, a server name shows only once, and the unread mark is no longer cut off.
- While you are in a voice call, RustTalk keeps you Online even when another app has the focus.
- Reopen the last chat picture instantly, even if the connection drops after it loads.
- Unavailable servers show an exclamation mark in the server sidebar until they reconnect.
- Recover voice calls and watched video after a server restart, keeping mute settings and asking again before shared audio plays.
- Phone calls sound clearer with HD voice (G.722) when your provider supports it, and fall back to normal phone quality otherwise.
- "Always allow" shares your microphone with every phone call without asking; turn it off again in Settings > Privacy.
- Others in the channel see a phone number you typed only as its last two digits; contacts show their name.

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
