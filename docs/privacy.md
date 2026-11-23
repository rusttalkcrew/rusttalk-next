# Privacy

No telemetry, analytics, crash reporting or tracking.

## What servers store

The API stores its Ed25519 signing seed in `server.key` and the following
tables in SQLite (WAL). IDs and digests are binary unless indicated. The `_ct` suffix denotes encrypted binary data.

| Table | Columns |
| --- | --- |
| `members` | `person_id` (root public key), `joined_at` (Unix seconds), nullable `display_name_ct`, nullable `avatar_ct`, `avatar_initialized` (whether a picture was explicitly set or cleared), nullable `bio_ct` (the profile bio), `presence_status` (current choice: online, away or invisible), `hide_offline` (whether to leave them out of member lists while offline), `voice_muted` (a moderator's server-wide voice mute) |
| `roles` | `id` (text), `kind` (built-in kind or custom), `rank`, `permissions` (fifteen-bit mask), nullable `name_ct` and `display_ct` (opaque ciphertext: the name, and the colour and member-list grouping) |
| `server_settings` | Singleton `id`, nullable `owner_person_id`, `default_role_id`, nullable `name_ct`, nullable `description_ct`, nullable `icon_ct`, `open_joining` (public flag), `member_cap` (open admission limit), nullable `index_listing_ct` (encrypted public-listing configuration), nullable `max_file_bytes`, `member_storage_bytes`, `server_storage_bytes`, `retention_secs`, `max_stickers` (server limits), `events_enabled` (whether Events is on) |
| `member_roles` | `person_id`, `role_id` |
| `invites` | `id` (opaque text), `code_hash` (SHA-256), `is_owner`, `remaining_uses`, `expires_at`, nullable `space_key_ct` and `name_ct` for preview, nullable `created_by` (person ID of the member who made it) |
| `challenges` | `nonce` (32 random bytes), `ip` (IPv4 address or IPv6 /64), `expires_at` |
| `sessions` | `token_hash` (SHA-256), `person_id`, `device_id` (Ed25519 public key), `ip` (canonical source IP at issuance), `expires_at` |
| `revocations` | `person_id`, `device_id`, `record` (W04 signature, device public key and revocation timestamp) |
| `bans` | `person_id`, `banned_at` (Unix seconds) |
| `ban_ips` | `person_id`, `ip` (IPv4 address or IPv6 /64; only for an explicitly requested IP ban) |
| `audit_log` | `id` (sequence), `actor_person_id`, `action` (fixed event kind), nullable `target_person_id`, nullable `target_id` (opaque object ID), `at` (Unix seconds), nullable `dm_channel_id` (audit visibility) |
| `invite_previews` | `ip` (IPv4 address or IPv6 /64), `expires_at` (60-second rate limit) |
| `open_joins` | `ip` (IPv4 address or IPv6 /64), `expires_at` (one-hour admission limit) |
| `member_devices` | `device_id` (Ed25519 public key), `person_id`, `certificate` (public keys, timestamp, salted name commitment, root signature and format prefix) |
| `categories` | `id` (text), `position`, `name_ct` |
| `channels` | `id` (text), `kind` (text, voice or dm), `page` (whether a text channel is shown as one page), `forum` (whether a text channel is shown as posts with replies), nullable `category_id`, `position`, nullable `name_ct` and `topic_ct`, `audio_kbps` (a voice channel's Opus bitrate, 8 to 128, plain because it is a setting rather than user-written content), `member_limit` (the most people in a voice channel at once, 0 for none, plain for the same reason), nullable `focus_until` (Unix seconds when focus mode ends, plain for the same reason), nullable `dm_first` and `dm_second` (person IDs) |
| `channel_ids` | `id` (text, including event IDs and deleted channel IDs to prevent reuse) |
| `calendar_events` | `id` (opaque text), `author` and `author_device` (public keys), `certificate` (public device certificate), `approved`, `epoch`, `remove_at` (Unix seconds: the end plus two hours, plain so the server can delete it), `envelope` (signed space-key ciphertext holding title, description, place, start and end), nullable `reminded_at` (Unix seconds) |
| `calendar_answers` | `event_id`, `person` and `device` (public keys), `certificate` (public device certificate), `epoch`, `envelope` (signed space-key ciphertext of going or not going, always the same size) |
| `channel_permissions` | `channel_id`, `role_id`, `flags` (four-bit mask) |
| `key_scopes` | `id` (text), `kind` (space or channel), nullable `channel_id`, `rotate` |
| `key_epochs` | `scope_id`, `epoch`, `commitment` (key commitment), `excluded` (public signing keys excluded from this epoch) |
| `key_envelopes` | `scope_id`, `epoch`, `recipient_device`, `sender_device`, `envelope` (HPKE ciphertext, encapsulation and signature) |
| `key_envelope_rejections` | `scope_id`, `epoch`, `recipient_device`, `sender_person` |
| `message_senders` | `device_id`, `person_id`, `certificate` (archived public certificate for signature verification) |
| `messages` | `sequence` (insertion order), `id` and `channel_id` (text), `epoch`, `sender_device`, `sender_person`, `at` (Unix seconds), `size` (binary envelope length), `envelope` (public framing, nonce, ciphertext with tag, and signature) |
| `message_ids` | `id` (opaque text, including deleted message IDs to prevent reuse) |
| `files` | `id` and `channel_id` (opaque text), `uploader` (person public key), `size` (ciphertext bytes), `created_at`, `expires_at` (Unix seconds) |
| `page_files` | `message_id`, `file_id` and `channel_id` (opaque text): the stored files a page version uses |
| `shared_files` | `id` (a stored file's opaque ID), `epoch`, `sender_device` and `sender_person` (public keys), `at` (Unix seconds), `envelope` (signed channel-key ciphertext holding the file's reference) |
| `emoji` | `id` (opaque text), `kind` (emoji or sticker), `owner` (person public key), `size` (picture ciphertext bytes), `image_hash` (short hash of the picture ciphertext), `name_ct`, `created_at` (Unix seconds) |
| `device_sync` | `person_id`, `version`, `ciphertext` (nonce and encrypted payload with tag), `size` (ciphertext bytes), `at` (Unix seconds) |
| `_sqlx_migrations` | `version`, `description`, `installed_on`, `success`, `checksum`, `execution_time` (migration bookkeeping) |

Caddy supplies the source IP directly. Session IPs are retained with the active
session so a moderator can choose an IP ban. Termination removes the session
row. An explicitly banned IP stays linked to its person ban until unban.
Challenge issuance prunes expired nonces, and sign-in deletes a consumed nonce.
Invite creation prunes expired invites, and redemption deletes spent ones.
Preview requests prune expired rate-limit rows. An open admission stores a
source address bucket without a person ID for one hour. A cleanup task
prunes expired admission rows every 30 seconds, and new admissions also
prune them.
The API persists public device certificates. The human-readable device
name and commitment salt stay on the device. Raw invite codes, link keys,
session tokens, space and channel keys, and readable names, descriptions,
topics, icons, avatars, bios and server emoji names and pictures are absent from API storage and logs. Audit
records carry IDs and fixed actions. The server knows DM participant IDs,
sender and routing IDs, ciphertext sizes, timestamps and access changes.
Text, replies, attachment references and edit targets are inside message
ciphertext. Explicit deletes and the retention cap remove message rows.
Public sender certificates and audit IDs are retained for history and
accountability.
The server knows who made an event or suggestion, whether it was approved,
and when it ends, who answered and when someone was last reminded. It never learns
the title, description, place, start or anyone's answer. Events
are deleted two hours after their end; their IDs stay reserved.
A DM is copied to every server both people share. Each of those servers
learns that the two people have a DM, plus the size and time of each copy
it stores, as it would for a DM on one server. Copies share a random
message ID, so servers that compare their data can tell which records are
the same message. A server that was offline misses messages sent meanwhile.
Friend requests and answers are encrypted DM messages, and the friend list,
blocks and the "Allow DMs from" choice live only in the encrypted sync blob.
Servers don't learn who is friends with whom, who is blocked, or that a
message is a friend request. They still see what they see for any DM: that
the two people have one, and each message's size and time. A friend message
is as large as a one-character text message.
Blocking and "Friends only" work in the app: servers keep delivering those
messages, and the app hides them or keeps them quiet.
Pokes are encrypted DM messages as well, so servers don't learn that a
message is a poke or what its note says; they see a DM message's size and
time. The poke history stays on the device (`pokes.json`) and is not synced.
Voice requests and their answers are encrypted DM messages too, so servers
don't learn that a message is one, which channel it names or what the answer
is. They see a small DM message and its time, which next to someone joining a
voice channel may suggest it. LiveKit sees a `rusttalk.ring` packet to each
device in the asker's room, so it learns that someone there rang and when, not
whom they asked. The packet is sealed with a key from the room's media key.
Who is muted stays in the app's memory and is not stored or synced.
Polls, votes and ending a poll are encrypted messages too: servers see a
message's size and time, not the question, the answers or who picked what.
A vote is a small message posted soon after a poll, and its size grows with
the number of answers picked.
The app stores on the device which person each DM channel is with, and the
newest epoch it trusts there with its commitment (`dm_pins/`), and its own
deleted DM messages (`dm_deleted.json`). Servers see each key link as one more
small DM message, sent just before a new epoch starts. A person's own deleted DM
messages are part of the encrypted device sync copy that every joined server
holds: their IDs and the addresses of the servers that held a copy.
The app keeps verified signed revocations in the profile. During device sync it
publishes its owner's revocations to each joined server, even for devices that
never signed in there. Members can see the person ID, revoked public device key,
signature and revocation time. A reader can also present that signed record to start an epoch for an affected scope on another joined server. Readers see the excluded public signing keys for that epoch. Device names stay private.
The API volume stores file blobs, including in-progress uploads and thumbnails,
as ciphertext under opaque disk names. Committed blobs are named only by their
random IDs. File names, types, keys, plaintext lengths and references linking
thumbnails to originals are inside the encrypted message. Server emoji and
sticker pictures are space-key ciphertext in `emoji/`, named by their random IDs. File operation logs
carry the opaque ID and ciphertext size. The expiry sweep unlinks blobs and
removes their metadata.
A file shared to a channel's file list stays until it is removed, its
channel is deleted, or its sharer leaves or revokes the device that shared it, so the API knows which stored files are kept that way, who
shared each and when. Its name and type stay inside the sealed envelope.
Saving a page version lists the IDs of the stored files it uses, so the API
knows which files each page version references (not their names, types or
which is a thumbnail) and keeps the newest version's files from expiring.
Each member has at most one current sync row per server. It is removed with
membership. Settings, joined origins and pins, and contacts are encrypted
on the device before upload. The sync key stays on the person's devices.
SQLite backups and old WAL pages can retain past
database contents, and volume backups can retain expired ciphertext.
The API knows which authenticated member and device have an open event socket,
including when a member chooses to appear offline. It keeps per-person socket
counts in memory and removes them one second after the last socket closes.
The current status choice (online, away or invisible) is stored with the
member row so an invisible member is never announced online when they
reconnect, use another device or the server restarts. Only the latest choice
is kept, and it is returned only to that member. Members see only the person
ID and visible status (online, away or offline). There is no last-seen
timestamp or presence event history. The existing session and connection-close logging
still applies.
A member can choose to be left out of member lists while they appear offline
(disconnected or invisible). The server stores this one boolean with the member
row and returns it with the member directory, so clients can still show the
member's name and picture on their messages, mentions and DMs; clients leave
the member out of the member list while their visible status is offline. The
admin member management view still lists them. This only hides the member in
other people's apps; it is not an access-control or anonymity guarantee.

Public listing is off by default. The owner explicitly supplies the name
and description they want to make public. The API encrypts this separate
configuration with a key derived from the local server key before writing
SQLite. The publisher opens it in memory for registration, and only the
owner can read it through the settings API. The private space metadata
continues to use the space key.

The index stores the intentionally public name, description, address, server
ID, member count and open-joining flag, plus the registration and expiry
timestamps used to enforce the lease and reject replays. It retains no
members' identities or signatures. It uses source IPs in memory for
registration rate limits, clears them each minute, and does not retain
search terms. Expired entries stop appearing in results immediately and
are later pruned from SQLite.

The API can see who is in which voice room by matching opaque LiveKit
device identities to current channel readers. Readers see unmatched
devices only as a count. After a kick, ban, revocation or permission
change the API asks LiveKit to remove devices that lost access and to
revoke any audio, video or data publication rights they lost (and to give back rights they regained, such as after a voice mute is lifted). It keeps the active
voice roster in memory for one second per room and drops expired rooms on the
next roster lookup. It keeps no voice roster or
LiveKit join tokens in SQLite. LiveKit sees opaque room and device IDs,
track IDs and source kinds, viewer subscriptions, routing and timing
information, negotiated video codec, dimensions and video layers,
ciphertext sizes, and encrypted media. It can tell that a device shared
a screen, a camera or an app audio track and see its encrypted track
label. Camera tracks carry the fixed label `camera`, never the device
name. Labels
use separate HKDF derivations from the room media key for app audio and
screen video, with a fresh nonce for each publication. They expose the
publication epoch and name length. The chosen monitor or window name,
its pixels, app names, camera pictures and audio are encrypted on the
sender. Capture target IDs, enumerated sources and cameras, and Linux
portal restore tokens stay on the sender's device.
LiveKit can see microphone mute state and the deafen attribute for each device in a voice room.
A moderator's voice mute is stored on the member and shown to every member. Removing or
moving someone from a voice room is logged by opaque channel ID; the move notice with
both channel IDs goes only to the moved devices.

Clients use the `rusttalk.video-watches` participant attribute for local
watch/unwatch sounds. It contains the IDs of video tracks that device is
receiving, encrypted with a separate HKDF derivation from the room media
key and authenticated against the viewer's LiveKit identity and key
epoch. Room clients can open it, and the publisher uses only entries for
its own tracks. LiveKit sees the attribute name, update timing, key epoch
and ciphertext length. It already sees video subscriptions for routing.
RustTalk's API receives no watch state, and sound preferences stay on the
device.

For remote control of a shared screen, LiveKit sees that two devices in a
room exchange data packets on the `rusttalk.control` topic, their sizes,
whether each is reliable or lossy, and their timing, which reveals roughly
when someone moves the pointer or types. Requests, answers and every input
event are encrypted and authenticated with a key derived from the room's
media key and bound to both devices, so LiveKit can't read, forge or
redirect them, and replayed input is dropped. Nothing about remote control is stored by the API, by
LiveKit or on either device.

For a shared terminal, LiveKit sees the `rusttalk.terminal` attribute
(that a device shares a terminal, and when), which devices watch it, and the
sizes and timing of the sealed data packets, which reveal roughly how much
the terminal prints and when. The output, the terminal's size and what
viewers ask for are encrypted and authenticated with keys derived from the
room's media key, and each message is bound to its sender and recipient.
Because those keys come from the channel's media key, a member of the
channel who also controls the media server could read a shared terminal or
pose as another device in the call, and so type in it. Requests to type,
answers and typed bytes are sealed the same way; LiveKit sees their sizes
and timing, which reveal roughly when someone types. Everyone watching sees
everything the terminal shows, so the app warns the first time someone
shares one. Nothing about a shared terminal is stored by
the API or LiveKit; the output lives only in each device's memory while it
is shown.

On Linux, RustTalk shares video through the ScreenCast portal. Hyprland
monitor shares can opt into control requests for that share, and the sharer
must still approve each request in RustTalk. The portal grants screen
capture permission only. Hyprland's Wayland virtual input protocols do not
show a separate permission prompt. The approved viewer can use apps visible
on the shared monitor. Window shares and other Linux compositors cannot
grant control.

RustTalk uses local Hyprland IPC to check monitor geometry, pointer position,
the active window, keyboard-interactive layer surfaces, lock state,
compositor keybinds, including mouse button and scroll binds, and the
keyboard layout options Hyprland matches binds with. It matches those binds
against the virtual keyboard's XKB state, assuming each bind needs exactly
its modifiers, because Hyprland doesn't report binds set to ignore them;
such binds are not detected. Hyprland doesn't report modifiers the sharer
is holding on a physical keyboard, so a modifier held by the sharer can
combine with remote input. Layer, bind and layout data stay
on the sharer's device, and RustTalk
does not collect what the sharer types. It pauses after observed local input
and rechecks the scope and binds about every 100 ms while a remote key or
button is held. If a check fails, RustTalk releases the held input and
destroys the virtual devices. Changes between a check and input delivery
can still route an event outside the shared area. Coincident local and
virtual input can share one idle notification.

On macOS, RustTalk captures displays and windows with ScreenCaptureKit,
which needs the Screen & System Audio Recording permission. Listing sources
reads the titles of on-screen windows of other apps; they stay on the
device except for the chosen name, which is encrypted like any share label.

For live files, only the encrypted offer is retained as a chat message.
The API sees its normal message metadata plus the live routing notice's
opaque transfer ID, offering and accepting device IDs, nonce, signature,
timing and whether the receiver accepted or cancelled. It relays that
notice in memory. Outside a call, the API can match short-lived data-room
participants back to authorized devices. LiveKit sees the channel's opaque
room ID, the file topic, opaque stream and transfer IDs, nonce, sender and
accepting device, ciphertext length and timing. The same access changes
that affect voice rooms remove data-room participants who lose read or
write access.
It relays encrypted chunks and keeps no file. The filename, MIME type
and file key are inside the encrypted message. File bytes never pass
through the API. Channel readers can decrypt the offer's metadata and
key, including from history. Only accepting devices receive the stream
from the relay. Receivers keep verified data in a local temporary file
during the transfer, remove it on failure or cancellation, and save the
complete file to the path chosen by the user.

The dev TLS certificate and private key live in `.dev/tls/`. Bot data under
`.dev/bots/` holds only saved origins, server IDs and TLS fingerprints.

## What the desktop client stores

The device signing secret, device agreement secret, sync key, person ID and
public device certificate live in Windows Credential Manager or Linux Secret
Service by default. Without a keychain (Steam Deck Game Mode) or by choice in
Settings > Privacy > Key storage they live in `identity.key` in the profile
folder instead: a file other normal users can't read (mode 0600 on Unix,
per-user AppData on Windows; administrators and software running as the same
user can), optionally locked with a password that is never stored (see
[Crypto](crypto.md#local-identity)). `keystore.json` records which of the three
holds the keys, whether keys were saved, and leftover copies still to remove.
Development profiles use separate entries. The client logs the person
fingerprint locally. Creating an account shows the recovery words once in the
app window. The client never stores or logs the words or the root signing key.
Copy on that screen puts them on the clipboard only when pressed: phones mark
the copy sensitive (Android hides it from the clipboard preview and keyboard
suggestions; iOS keeps it off Universal Clipboard and lets it expire after two
minutes), and desktops clear the clipboard after two minutes if it still holds
the words and the app is still running.
After the device keys are unlocked, the app can show its last server directory,
icons, avatars and recent messages without contacting the server. These local
files are authenticated and encrypted with a key derived from the unlocked device identity and bound to
the device and pinned server. The app checks locally known device revocations
before opening messages. Changes made while the device is offline become
visible when it reconnects. Removing a server clears its files.

"Sign out of this device" deletes the keychain entry, `identity.key`, leftover
temporary key files and the profile's `sync.json`, `servers.json`,
`saved.json`, `notify.json`, `pokes.json`, `dm_deleted.json`, `dm_pins/`,
the encrypted recent-view cache, recent and favourite emoji, cached link previews and both local logs. This
clears the previous person's fingerprint, DM history markers, server records
and preview consent. Hardware, local sound and window preferences in
`voice.json`, the downloaded noise removal model and updater settings stay on
the device.

Next to the settings, `servers.json` holds each server's address, server ID
and TLS fingerprint. On Android, an empty `unsent` file means the app had to
end while messages were still being sent; the app deletes it once it has told
the person, in a chat. Desktops never write or read it. `sync.json` holds the last merged device sync snapshot
with its version: joined servers with their pins, servers left, synced
settings, friends (with pending requests, blocks and each person's last seen
name), and local edits not yet uploaded. `voice.json` holds the
chosen audio devices, voice options, the chosen camera's name,
resolution and frame rate, and the server directory address and
certificate pin used by "Find servers" (`index_url`, `index_fingerprint`),
the server IDs of official servers dismissed from the join page
(`dismissed_official`), and the window layout (sidebar widths, the member
list, and how a voice channel splits its call and chat). Of these, only the voice options (push to talk
and its key, activation level, echo cancellation, noise suppression, auto
gain), the camera resolution and frame rate, the dismissed official
servers, "Allow DMs from" (`friends_only`), the default display name, the
bio and the servers with a name of their own sync to the person's other
devices; the chosen microphone, speakers and camera stay on this device.
`voice.json` also lists the servers this device still has to send a name or
bio change to. The default name and the bio are sealed for each server like
other member names, so only members of servers the person is in see them;
friends see them on the servers they share.
`notify.json` holds the notification choices (desktop notifications, sound,
"only when not focused", and per server address and channel ID the level,
@everyone suppression and mute end time) and, per channel ID, the time of the
newest message read. Both sync to the person's other devices inside the
encrypted sync blob. Desktop notifications show the sender, channel and a short
preview decrypted on this device; they go only to the operating system's
notification service (D-Bus on Linux, toasts on Windows, the notification center on macOS). Decrypted names, messages and pictures stay in
memory only; the log never contains them, file names or invite links. A
downloaded file is written only where the person picks in the save dialog,
through a temporary file in that folder that is removed if the download fails.
`saved.json` maps each downloaded file's opaque blob ID to where it was saved,
so "Show in folder" and "Open" work later; it never leaves this device.
"Open in default app" in the picture viewer, for a picture not saved yet,
writes it to a `rusttalk-<id>` folder in the system temporary directory and
hands it to the system's default app; the client deletes these folders when it
starts and when the person signs out. A picture pasted into the composer
(Ctrl+V) is written the same way, to a `rusttalk-<random>` folder, so it can be
sent like a picked file; removing it from the message deletes it, and otherwise
it goes with the other folders. The client reads the clipboard only when the
person pastes, and on desktops once more two minutes after copying the recovery
words: it empties the clipboard only if it still holds them (read, compared by
hash and emptied in one step; nothing is kept), and a newer copy in the app
cancels that.
`emoji-recent.json` holds up to 32 emoji selected through the picker or
autocomplete, and `emoji-favourites.json` up to 64 emoji marked as favourites
in the picker. Both stay on this device.
`updates.json` holds the chosen release directory URL and the automatic
check preference (on by default in release builds) on this device.
`route.json` holds this device's network routing choice (Direct, SOCKS5, WireGuard,
or Tor) and non-secret endpoints, addresses, DNS servers, and AllowedIPs; it stays
on this device and is never included in encrypted device sync or removed on sign-out.
Route credentials (SOCKS5 password, Tor SOCKS5 password, WireGuard private key and
preshared key) are never written to `route.json` or logs: desktops store them in the
OS keychain (or a mode-`0600` `route.secret` file when no keychain exists, such as
Steam Deck Game Mode); Android wraps them with Android Keystore AES-256-GCM in
`route.wrapped`; iOS stores them in the data-protection Keychain
(`WhenUnlockedThisDeviceOnly`).
`deepfilternet3.tar.gz` is the AI noise removal model, present only after the
person downloads it; `voice.json` keeps whether it's on (`ai_noise`), on this
device only.

### AI noise removal

AI noise removal (Settings → Voice & audio) runs the DeepFilterNet3 model on
the person's own computer, in the client's microphone pipeline. Microphone
audio never leaves the device for it: what it cleans goes out only in the
normal encrypted call. The model is not part of the app. It is downloaded only
when the person selects **Download**, from a fixed upstream commit (see
Outbound connections), and the client checks its size and pinned SHA-256
before saving it and again every time it loads it. A file that fails the check
is deleted and never used; the normal noise suppression keeps working.

## Outbound connections

- **Bot:** Only the configured RustTalk stack. Membership, key events, encrypted
  chat, device sync, signaling and failure-path validation use Caddy on 8443/TCP.
  WebRTC uses the advertised LiveKit node on
  7882/UDP or 7881/TCP. The bot requests a voice token from the pinned API.
  The index e2e scenario uses pinned HTTPS to search the dev index on the
  same edge.
- **Desktop client:** Configured RustTalk origins for pinned HTTPS and WebSocket
  connections. Voice uses the same pinned signaling path and advertised
  WebRTC ports as the bot. The client contacts the server directory only
  when the person searches in Find servers: the address set in Settings →
  Privacy, or else the official directory (`index.rusttalk.com`) with normal certificate trust. The join page lists the
  official servers (`es`, `de` and `jp.public.rusttalk.com`) from a list
  built into the client; it contacts one only when the person picks Join. An
  invite link or server address typed on the join page is contacted only when
  the person picks Continue. An invite link in a chat message shows as a
  card built only from the link itself (the server address) or, for a server
  the person is already in, from what the device already knows (its name
  and icon). Viewing the card contacts nothing. The invited server is first
  contacted when the person picks Join, which makes the same invite preview
  request as Continue on the join page; the server's name comes from that
  preview, and joining still needs confirmation. There is no separate
  "details" request: the preview is the only thing the server reveals before
  joining, and asking for it earlier would leak the same IP address and timing
  as Join. On Linux it accesses Secret
  Service, and opens file dialogs through xdg-desktop-portal, over the local
  session bus.
- **Update host:** The app contacts the HTTPS directory configured in
  Settings (by default the GitHub release directory of the RustTalk
  repository, `github.com`) at startup, on the chosen schedule (every 15
  minutes by default) and when you select
  **Check now**. Turn the automatic checks off in **Settings → Updates**
  (**Check automatically**). It requests the release manifest and its detached
  signature, then requests your platform’s artifact if you choose to update.
  The host sees your IP address, requested files and timing. Requests carry no
  account, device or server identifiers. Redirects stay on HTTPS.
- **Previews of links you send (asked first):** Off until you agree. When a
  message you send (or a page you save) links to a website that no reader-side
  link preview below covers, the app asks "Create a preview? Your app will visit <host> to
  make it." with **Always**, **Not now** (send without, ask again next time)
  and **Never**; Settings → Privacy → Link previews (**Create previews for
  links I send**) changes the choice. Until you pick Always nothing is
  requested. With it on, your app visits that link once while sending, to
  make a preview of at most one link per message: it reads up to 512 KiB of the page for its Open Graph, Twitter
  card or plain title and description, then downloads the picture it names.
  The preview travels end-to-end encrypted with the message as a small file
  with a thumbnail (see [Protocol](protocol.md#link-previews)), so readers and
  the RustTalk server never contact the site. Requests go only to public
  addresses: the client resolves the name itself and refuses private,
  loopback, link-local, shared, multicast and reserved addresses, checks
  every redirect again (at most three), uses no proxy, sends a generic
  User-Agent and no cookies or Referer. Fetching stops after five seconds, and
  the whole preview, upload included, after eight; the message then goes
  without it and a partial upload is removed. Removing the preview, turning
  the setting off or signing out stops a fetch under way before its next
  request. The site sees your IP address, timing and the link. While the
  preview is made the composer shows "Adding preview…" with a button to send
  without it. Readers show a card only when its address is one of the
  message's own links.
- **Link previews (opt-in per site, per device):** Off until the person allows a
  site, either on the first card that asks ("Show Steam previews?") or in
  Settings → Privacy → Link previews, and off again there. Until then the
  link stays a plain link and nothing is requested. Once allowed, the app
  showing a message (or a page) fetches the preview itself, only for messages on screen;
  the sender and the RustTalk server are not involved. Each site has its own
  question and toggle, and is asked only for its own kind of link:
  - **Steam**, `store.steampowered.com/app/<id>`: requests
    `https://store.steampowered.com/api/appdetails` with the app ID, the app's
    language and the computer's country (for local prices); the header picture
    from `*.steamstatic.com` or `steamcdn-a.akamaihd.net`.
  - **YouTube**, `youtube.com/watch`, `youtu.be` and `youtube.com/shorts`:
    requests `https://www.youtube.com/oembed` with the video's address; the
    thumbnail from `i.ytimg.com`.
  - **Spotify**, `open.spotify.com` tracks, albums, playlists and episodes:
    requests `https://open.spotify.com/oembed` with the item's address; the
    cover from `i.scdn.co` or `*.spotifycdn.com`.
  - **GitHub**, repositories, issues and pull requests on `github.com`:
    requests `https://api.github.com/repos/...` without an account; the avatar
    from `avatars.githubusercontent.com`. GitHub allows 60 such requests an
    hour per IP address, so a rate limit pauses GitHub previews until the time
    GitHub gives.
  - **Wikipedia**, `<language>.wikipedia.org/wiki/<title>`: requests
    `https://<language>.wikipedia.org/api/rest_v1/page/summary/<title>` of the
    link's own language; the picture from `upload.wikimedia.org`.
  - **Reddit**, `reddit.com/r/<subreddit>/comments/<id>`: requests the post's
    `.json` from `https://www.reddit.com`; the picture from `i.redd.it`,
    `preview.redd.it` or `external-preview.redd.it`, never for posts marked
    adult (those show an NSFW label).

  Requests are HTTPS only, go only to the hosts above, follow no redirects,
  and send a generic User-Agent, no cookies and no Referer. They have a
  timeout and size caps (pictures are also capped in size before decoding),
  start at most one per second with two at a time, and after a failure or a
  rate limit the whole site waits ten minutes or as long as it asks. A missing
  page only affects that link. A request still queued is dropped when its link
  scrolls off screen or the site is turned off, and one under way when it is
  turned off keeps nothing. Each site sees your IP address, timing and which
  links you look at; requests carry no account, device or server identifiers.
  Previews are kept in memory (the 50 most recently shown) and in `previews/`
  in the app's data folder for up to a day (refreshed after an hour for Steam
  prices, twelve hours for the others), at most 200 of them. Signing out stops
  all of it and deletes `previews/`.
- **Pasted web pictures:** When the person pastes a picture copied from a web
  page and the browser put only a still copy of a moving picture (GIF or WebP)
  on the clipboard, the desktop client downloads the original over HTTPS from
  the address the page gave, as the browser already did. That site sees the IP
  address and timing. The request carries no account, device or server
  identifiers.
- **AI noise removal model (opt-in):** Only when the person selects
  **Download** in Settings → Voice & audio, the desktop client fetches
  `models/DeepFilterNet3_onnx.tar.gz` of `Rikorose/DeepFilterNet` at commit
  `d375b2d` from `github.com`, which redirects to `raw.githubusercontent.com`
  (HTTPS only). GitHub sees your IP address and timing. The request carries no
  account, device or server identifiers and no audio.
- **Phone calls (opt-in, desktop):** Nothing happens until the person saves a
  SIP account in Settings → Phone. Calling a number from a voice channel then
  connects to that account's SIP server only (SIP over UDP, no TLS): the
  client signs in with the account, calls, and signs out after the call. The
  call's sound goes to and comes from the media server the provider names, as
  G.711 over RTP. **This is not end-to-end encrypted**: the provider, and
  anyone on the network path, can hear the call, and sees the number, the
  account and the IP address. The dialer says so. While the call runs, the
  caller's app shares their own microphone and only the microphones of members
  who explicitly choose “Share my microphone with this call”. Each member sees
  an external-call warning before choosing. This permission is separate from
  Listen, bound to the call publication and device session, and withdrawn when
  stopped, disconnected, or the call ends. Newcomers and older clients that cannot
  consent are silent to the phone. Other shared tracks are never exported.
  Consenting voices retain full volume even if the caller lowers their local
  volume or deafens; the caller's microphone obeys mute, deafen and push-to-talk.
  Phone sound is published as encrypted shared audio; members press Listen to
  hear it. Call notices and consent are encrypted participant attributes bound
  to the sender and session, with authenticated increasing revisions preventing
  replay of withdrawn grants. Stopping sharing immediately pauses the channel
  microphone until the bridge confirms exclusion (or the call ends); a failed
  confirmation keeps it paused with a visible notice. This brief pause also
  affects other listeners in the channel, without changing mute or push-to-talk.
  A directly dialed local phone call is independent of this transmission pause.
  Losing permission or reconnecting ends the phone
  call. Media accepts only the negotiated UDP endpoint and G.711 payload; hold,
  one-way audio and non-RTP/AVP SDP are rejected. The RustTalk
  server never sees the number or the sound. The account (server, username, caller
  name) and the contacts are kept in `voice.json` on this device only and are
  never synced; the password is kept in the OS keychain (or only in memory
  until the app closes, without one). Logs never name the number, account or
  password.
- **API:** Queries LiveKit RoomService on 7880/TCP using the internal
  Docker hostname `livekit`. When the owner enables public listing and
  saves an index URL, the API posts a signed listing to that HTTPS URL at
  opt-in, on startup and hourly while enabled. The owner's settings prefill
  the official directory, which the owner can change; the API itself has no
  default index URL.
- **Server auto-updater (opt-in):** Fetches the signed release manifest and
  detached signature from GitHub Releases over HTTPS. Docker pulls the
  selected digests from GitHub Container Registry. These hosts see the IP
  address, requested assets or images, and timing. The updater queries the
  internal LiveKit RoomService for room activity without retaining room IDs,
  and checks health and version over the internal network. It stores pending
  and last release versions, timestamps and the last result locally for the
  server owner, and keeps local backups of the server data volumes.
- **Index:** No outbound network connections.
- **Caddy:** Proxies requests to the API on 8080/TCP and to LiveKit on
  7880/TCP, using Docker's internal DNS. With the optional index enabled,
  Caddy also proxies the directory path to the index on 8080/TCP. Dev TLS
  uses a local certificate.
- **LiveKit:** Replies to Caddy and to room participants over their WebRTC
  transports. LiveKit posts signed join, leave and room-end webhooks to
  `http://api:8080/internal/livekit` for presence notifications. This route
  stays behind the Compose network. Caddy exposes the public API and
  signaling paths.
  The dev health checks use loopback HTTPS at Caddy and loopback
  HTTP at LiveKit.

Mobile updates are manual. Downloading a release contacts GitHub through
your browser. On iOS, your sideloading tool handles signing with your Apple
account. RustTalk does not receive those credentials or check for updates on phones.

Docker image pulls and Cargo dependency downloads happen during setup or build.
WebRTC builds download the pinned LiveKit prebuilt archive from GitHub
Releases. Linux release builds let appimagetool fetch the AppImage runtime
from GitHub Releases when no local runtime is supplied.

## LiveKit

The dev stack uses the unmodified `livekit/livekit-server:v1.13.11` image
without `--dev`. Compose passes `NODE_IP` as `rtc.node_ip`. The config sets
`rtc.use_external_ip: false` and `rtc.use_ice_lite: true`. An explicit node IP
skips IP discovery. ICE-lite skips server STUN gathering, and the explicit IP
is advertised on `rtc.udp_port: 7882` and `rtc.tcp_port: 7881`.
[LiveKit command flags](https://github.com/livekit/livekit/blob/v1.13.11/cmd/server/main.go#L70-L74),
[pinned RTC config](https://github.com/livekit/mediatransportutil/blob/dd8e31a0adc0/pkg/rtcconfig/config.go#L108-L133),
[WebRTC setup](https://github.com/livekit/mediatransportutil/blob/dd8e31a0adc0/pkg/rtcconfig/webrtc_config.go#L91-L116),
[ICE-lite and STUN defaults](https://github.com/livekit/mediatransportutil/blob/dd8e31a0adc0/pkg/rtcconfig/webrtc_config.go#L223-L235),
[LiveKit ports](https://docs.livekit.io/transport/self-hosting/ports-firewall/).

The open-source server constructs its analytics service without any reporting
clients, so its send methods return immediately. The dev config leaves Redis,
Jaeger tracing, Prometheus, TURN and mDNS disabled. The server's
startup path has no update check.
[Analytics implementation](https://github.com/livekit/livekit/blob/v1.13.11/pkg/telemetry/analyticsservice.go#L66-L110),
[startup](https://github.com/livekit/livekit/blob/v1.13.11/cmd/server/main.go),
[sample config](https://github.com/livekit/livekit/blob/v1.13.11/config-sample.yaml).

With STUN and TURN left empty, LiveKit still advertises public Google and Twilio
STUN servers in signaling. Rust SDK 0.11.0 takes that list when
`RoomOptions.rtc_config.ice_servers` is empty. RustTalk removes the advertised
list from join and reconnect responses in its pinned signaling transport.
The bot then uses host candidates for direct ICE checks against LiveKit.
This covers RustTalk's pinned transport. Other SDK clients can still receive
the server's default list.
[Server ICE list](https://github.com/livekit/livekit/blob/v1.13.11/pkg/service/roommanager.go#L1150-L1157),
[default STUN endpoints](https://github.com/livekit/mediatransportutil/blob/dd8e31a0adc0/pkg/rtcconfig/config.go#L38-L41),
[SDK ICE selection](https://github.com/livekit/rust-sdks/blob/livekit/v0.11.0/livekit/src/rtc_engine/rtc_session.rs#L2880-L2905).

### W03 traffic check

On Linux, after a fresh `make dev`, both `--once` joins reached the WebRTC data
channel with the pinned certificate. One used `127.0.0.1:8443` for signaling,
and one used the host LAN IP on 8443. A wrong pin exited 1 with a certificate
fingerprint mismatch and left no LiveKit participant.

- `tcpdump -nli any -s 128 'ip or ip6'` in the LiveKit container's network
  namespace showed only Caddy traffic on 7880/TCP, the bot's media on
  7882/UDP via Docker's host gateway, and loopback health checks.
- `strace -f -s 0 -yy -e trace=connect,sendto,sendmsg,sendmmsg,write,writev`
  on each bot showed signaling to the requested IP and UDP sends only to
  the LAN node IP on 7882. WebRTC used two temporary UDP sockets for local
  route selection against `8.8.8.8:53` and `[2001:4860:4860::8888]:53`.
  Neither socket sent data. Host `tcpdump` captured zero packets to or from
  either address during the checks.

## Per-app network routing

By default RustTalk connects **Direct**. On first start (before RustTalk makes its
first network request, including automatic update checks) and in **Settings →
Network** on Linux, macOS, Windows, Android, and iOS, a person can choose **Direct**,
**SOCKS5**, app-only **WireGuard**, or **Tor**. See [Routing](routing.md) for setup
and platform constraints.

- **Scope:** Only RustTalk's own traffic uses the chosen route. RustTalk does not
  modify system routing tables, OS VPN profiles, or other apps' traffic. Clicking a
  link in chat, a preview card, or release notes opens the system web browser, which
  uses the browser's own network settings.
- **Permitted direct connections while routed:** Only the connection required to
  establish the selected route itself (TCP to the SOCKS5 or Tor SOCKS5 endpoint, or
  UDP to the WireGuard peer endpoint, plus local DNS resolution of the proxy/peer
  hostname if not given as an IP literal). OS/environment proxy variables are
  ignored (`.no_proxy()`).
- **Fail closed:** On onboarding (until confirmed), cold start while connecting,
  missing/unreadable credentials, proxy/tunnel failure, or WireGuard handshake
  expiry, RustTalk enters **Blocked** and refuses outbound traffic instead of
  silently falling back to Direct. Returning to Direct requires an explicit user
  choice.
- **Preserved guarantees:** End-to-end encryption (messages, files, metadata, sync)
  and TLS certificate pinning (`PinnedCertificate` and server identity proofs) are
  enforced identically over Direct, SOCKS5, WireGuard, and Tor.
- **DNS isolation:** SOCKS5 and Tor always use `socks5h` remote DNS (`ATYP=0x03`)
  so destination hostnames (including `.onion` addresses on Tor) are never sent to
  local OS DNS. For optional sender-side link previews under SOCKS5 or Tor,
  RustTalk blocks literal private IPs and local hostnames before contacting the
  proxy, but cannot inspect what IP a remote proxy resolves a public-looking
  hostname to (enforcing private-network egress rules on the proxy host is up to
  the proxy/Tor operator). App-only WireGuard resolves destination hostnames
  exclusively over UDP inside the encrypted WireGuard tunnel using the configured
  WireGuard `DNS` servers (and refuses domain lookups if no tunnel DNS server is
  configured); for sender-side link previews it validates the returned public IPs
  and pins them directly over the tunnel bridge so DNS rebinding cannot occur.
- **Calls, live file transfers, and SIP:** Because `livekit`/`libwebrtc` and
  `rsipstack` open direct OS UDP/TCP sockets and gather local host ICE candidates
  without a routed socket-factory hook, voice/video calls, screen sharing, shared
  terminals, remote control, LiveKit live file transfers, and desktop SIP phone
  calls are blocked whenever SOCKS5, app-only WireGuard, or Tor is active.
  Server-stored encrypted file transfers (up to 32 MiB) remain available over HTTPS.
- **What operators can observe:**
  - **SOCKS5 proxy operator:** Sees your IP address, timing, traffic volume,
    destination hostnames and ports (because of remote DNS), and TLS SNI / handshake
    metadata. Cannot read E2EE RustTalk content or break server certificate pins.
    For opt-in link previews, pasted web pictures, or updates, sees the requested
    destination hosts and timing.
  - **WireGuard peer operator:** Sees your IP address, WireGuard public key,
    encrypted tunnel timing/volume, inner destination IPs/ports, in-tunnel DNS
    queries sent to your chosen DNS server, and TLS SNI. Cannot read E2EE RustTalk
    content or break server certificate pins. Your local ISP sees only encrypted
    WireGuard UDP packets to the peer endpoint.
  - **Tor network:** Your local ISP and guard relay see that you connect to Tor (and
    timing/volume), not your destinations. For clearnet servers, the Tor exit relay
    sees destination hostnames, ports, TLS SNI, and timing (not E2EE content or
    pinned-TLS plaintext). For `.onion` servers, traffic stays inside the Tor
    network end to end. If you connect to a remote Tor SOCKS5 proxy instead of
    localhost (`127.0.0.1`), that proxy host sees the same metadata as a SOCKS5
    proxy.

### Routing traffic check (Linux isolated network namespace)

On Linux, running the `rusttalk-core` controlled-endpoint routing test suite inside an
isolated `unshare -rn` network namespace with a `dummy0` default route (`198.51.100.10/24`,
`fd99::10/64`) and a concurrent unrelated direct TCP client, while capturing all frames
with `AF_PACKET` (`SOCK_RAW`, `ETH_P_ALL`) across `lo` and `dummy0`, observed:

- **381 total frames captured** across SOCKS5h (IPv4/IPv6/WebSocket/pinning/failure),
  Tor SOCKS5h (`.onion` remote DNS with stream isolation), and userspace WireGuard
  (`boringtun` + `smoltcp` dual-stack IPv4/IPv6, in-tunnel UDP DNS, AllowedIPs, pinning):
  - `0` external frames on `dummy0`;
  - `0` host DNS packets (port 53 TCP or UDP);
  - `0` unencrypted packets to/from WireGuard inner addresses (`10.77.0.0/24`, `fd77::/64`)
    on the OS network stack (all 44 UDP packets on `lo` were encrypted WireGuard frames with
    `0` plaintext marker leaks);
  - `0` WebRTC/STUN/SIP packets (ports 3478, 5060, 7881, 7882).
- IPv4 and IPv6 routing tables remained identical before and after the run, and the
  concurrent unrelated direct TCP process completed all exchanges without errors.
- **Unverified platforms:** Runtime and packet-capture verification has not yet been
  performed on macOS, Windows, Android, or iOS devices (see [Routing](routing.md)).
