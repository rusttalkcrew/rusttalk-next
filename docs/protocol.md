# Protocol

The API runs over HTTPS. Requests with JSON bodies use `Content-Type: application/json`.
The default request body limit is 4096 bytes. Binary fields use canonical unpadded
base64url. Times are Unix seconds. Auth and membership responses carry
`Cache-Control: no-store`.

## Release compatibility

The public `GET /v1/server-info` response reports the server's release
`version` and integer API `protocol`. The app supports protocols 1 through 3.
Protocol 2 adds message format v3. Protocol 3 enables protected key epochs.
If the endpoint returns 404 or the protocol field is missing, the app assumes
protocol 1. The app reads this information over the verified connection.
Protocol metadata never changes TLS or server pin checks, device authentication,
or encryption.

Keep adjacent releases working together in both directions. Add optional
fields and endpoints without changing the protocol number. If a feature's
endpoint is unavailable, the app should disable that feature and explain
that the server needs an update. For example, a server without channel
ordering leaves the existing order in place and the app disables the move
controls after the first unsupported response.

Increase `protocol` only for a breaking API change. Stage such changes over
multiple releases so the new server still serves the previous app's API and
the new app still understands the previous server's API. Ship client support
before a server advertises a protocol outside the previous app's range.
Below the supported range the app warns that the server needs updating and
tries to connect. Above it the app directs people to Settings → Updates.

## Sign-in

| Method and path | Request | Success |
| --- | --- | --- |
| GET `/v1/health` | Empty | 200, `ok` |
| GET `/v1/server-info` | Empty | 200, `{open_joining, version, protocol, dm_backfill}` |
| GET `/v1/auth/challenge` | Empty | 200, `{challenge, server_id, signature}` |
| POST `/v1/auth/sign-in` | `{protocol, person_id, certificate, challenge, signature, invite?}` | 200, `{token, expires_at}` |
| POST `/v1/auth/join` | `{protocol, person_id, certificate, challenge, signature}` | 200, `{token, expires_at}` |
| POST `/v1/auth/refresh` | Bearer token, empty body | 200, `{token, expires_at}` |

The server signs the 32 raw bytes of the challenge followed by the ASCII
bytes `rusttalk-server-v1`. The client verifies the Ed25519 signature against
the expected 32-byte server ID, then signs these bytes with its device signing key:

```text
challenge (32 bytes) || server_id (32 bytes) || "rusttalk-auth-v1" (16 ASCII bytes)
```

Domain strings have no trailing terminator. Signatures are 64 bytes. The
certificate uses the public W06 format described in [Crypto](crypto.md). Use a fresh
challenge per attempt. A client may sign the challenge it verified while connecting
instead of fetching another, but only once and only within 30 seconds of requesting it. The server consumes a recognized challenge before
verifying the device proof, and rejects challenges after 60 seconds. At most
4096 challenges are outstanding across the server and 64 per IPv4 address or
IPv6 /64; a new challenge past an address's share replaces that address's oldest.

Members sign in with their device proof. For a new member, supply the invite
code as the optional `invite` field. Invite redemption, membership and session
creation are atomic. Each person and device pair can have one current session.
Successful sign-in replaces the previous session for the device and closes
its event sockets. A refresh rotates the bearer token and extends expiry to
12 hours from the refresh while preserving the session's open event sockets.
A person may register up to ten devices. At the cap, sign-in from a new
device returns `409 device_limit`. Revoking a device frees a slot.

### Open joining

After verifying the server ID and TLS pins, read the public `GET /v1/server-info`
flag before asking the person to join. Send a fresh device proof to
`POST /v1/auth/join` to request admission without an invite. An ordinary
`sign-in` still requires an invite for nonmembers. Person and IP bans apply
to both join methods.

Open joining starts disabled. An Owner or Admin can enable it and set a
positive member cap, which defaults to 100. The cap counts all current
members, including the owner, and applies to admission without an invite.
At the cap, invitations and existing member sign-ins still work. Successful
open admissions are limited to five per IPv4 address or IPv6 /64 and 25
across the server in a rolling hour. An open join receives the current
default role, and online members wrap the space and channel keys for the
new device.

## Invite links and server pins

```text
rusttalk://<host>[:port]/i/<code>#<server_id>[.<tls_sha256>][.k~<link_key>]
```

Use a hostname, IPv4 address or bracketed IPv6 address. Port defaults to 443.
The code is 16 random bytes. The fragment carries the full 32-byte Ed25519
server ID, followed in self-signed mode by the 32-byte SHA-256 hash of the DER
leaf certificate. Public CA mode omits the TLS field and uses normal TLS
verification. The optional `.k~` field holds the 32-byte invite link key.
Owner bootstrap links and pre-W06 links omit it. Pass the invite code in
`POST /v1/invite-preview` before sign-in. The API returns the encrypted
space key and name snapshot. The client reads the link key from the fragment,
checks the server and TLS pins, and decrypts locally.

Pass a link through a trusted channel and give the HTTPS API only its code,
inside the sign-in or invite-preview body. Clients pin the verified server ID and any explicit
TLS fingerprint against the canonical HTTPS origin. A mismatch at a known
origin is a hard failure. For first use by address, the desktop UI will show
the server fingerprint for comparison. Dev bots use the local TLS certificate
or the fingerprint supplied by the operator.

## Membership and administration

All endpoints below require `Authorization: Bearer <token>`.

| Method and path | Body | Permission and success |
| --- | --- | --- |
| GET `/v1/me` | Empty | Any member. 200, `{person_id, device_id, joined_at, role_ids, status, hide_offline}`; `status` is the caller’s own presence choice |
| PUT `/v1/me/hide-offline` | `{hide_offline}` | Any member. 204. Stores whether member lists leave the caller out while their visible presence is `offline`, and sends `member_changed` |
| PUT `/v1/me/status` | `{status}`, one of `online`, `away`, `invisible` | Any member. 204. Stored and applied to the member’s event sockets |
| DELETE `/v1/me/membership` | Empty | Any member except the owner. 204, leaves the server like a kick: sessions close, keys rotate, audit `left`. The owner gets `409 owner_cannot_leave` |
| POST `/v1/me/revocations` | `{record}`, a W04 root-signed revocation for the caller's person | Any member. 204, affected device loses its token |
| POST `/v1/invites` | `{uses, ttl_seconds, space_key_ct, name_ct?}` | `invite`. 201, `{id, code, expires_at}` |
| GET `/v1/invites` | Empty | `invite`. 200, active invites as `{id, created_by, remaining_uses, expires_at}`, soonest expiry first: the caller's own, or everyone's with `manage_server`. Invites whose maker no longer holds `invite` are left out |
| DELETE `/v1/invites/{id}` | Empty | `invite` for the caller's own invite, also `manage_server` for anyone else's. 204 |
| DELETE `/v1/members/{person}` | Empty | `kick`. 204 |
| PUT `/v1/members/{person}/voice-mute` | Empty | `mute_members`. 204, mutes them in every voice channel until lifted |
| DELETE `/v1/members/{person}/voice-mute` | Empty | `mute_members`. 204, lifts the mute |
| GET `/v1/bans` | Empty | `ban`. 200, active bans as `{person_id, banned_at, ban_ip, can_lift}`, newest first |
| PUT `/v1/bans/{person}` | `{ban_ip?}`, default false | `ban`. 204 |
| DELETE `/v1/bans/{person}` | Empty | `ban` and may lift it (see below). 204 |
| GET `/v1/roles` | Empty | Any member. 200, `{default_role_id, roles}` |
| POST `/v1/roles` | `{id?, rank, permissions, name_ct?}` | `manage_roles`. 201, the created role |
| PUT `/v1/roles/{id}` | `{permissions, name_ct?, rank?}` | `manage_roles`. 204, replaces a custom role's flags and, when given, its name and rank (1 to 999, below the caller's) |
| PUT `/v1/roles/{id}/name` | `{name_ct?, display_ct?}` | `manage_roles`. 204. Each field left out is kept; `null` clears it. `display_ct` seals `[1, flags, r, g, b]` (flag 1 colour, 2 listed separately) as `RoleDisplay` metadata |
| DELETE `/v1/roles/{id}` | Empty | `manage_roles`. 204 |
| PUT `/v1/roles/default` | `{role_id}` | `manage_roles`. 204 |
| PUT `/v1/members/{person}/roles` | `{role_ids}` | `manage_roles`. 204, replaces assigned roles |
| PUT `/v1/owner` | `{person_id}` | Owner. 204, transfers ownership to a member |
| GET `/v1/settings/open-joining` | Empty | Owner or Admin. 200, `{open_joining, member_cap}` |
| PUT `/v1/settings/open-joining` | `{open_joining, member_cap}` | Owner or Admin. 204 |
| GET `/v1/settings/files` | Empty | Any member. 200, `{limits, ceilings, used_bytes}` |
| PUT `/v1/settings/files` | `{max_file_bytes, member_storage_bytes, server_storage_bytes, retention_secs, max_stickers}` | `manage_server`. 200, updated `{limits, ceilings, used_bytes}` |
| GET `/v1/audit?after=0&limit=100&action=member_banned` | Empty | `view_audit_log`. 200, matching audit entries in ascending ID order |

`limits` and `ceilings` each contain the five fields accepted by PUT. Every
field must be a positive integer, or PUT returns `400 invalid_file_limits`.
Host ceilings are also the defaults for unset limits. The server clamps
limits to host ceilings on save and on read. `used_bytes` counts all committed
file ciphertext, including expired files that cleanup has yet to remove.

A role has `{id, kind, rank, permissions, name_ct}`. A member's permissions
are the union of their role flags, and their highest rank is used for
hierarchy. Owner (1000) and Admin (100) have all sixteen flags. Moderator (50)
has `kick`, `ban`, `delete_messages`, `mute_members`, `move_members`, `suggest_events`,
`create_events`, `manage_events` and `start_focus`. Member (0) has `suggest_events`
and `start_focus` and starts as the default. Every new role gets `start_focus`
when its creator has it. The other flags are `invite`, `manage_roles`,
`view_audit_log`, `manage_channels`, `manage_server`, `mention_everyone`, and
`manage_emoji`. Built-in role IDs match their kinds. Custom roles have opaque IDs and ranks
from 1 to 999, strictly below their creator. They can only receive flags that the creator
has. `name_ct` is null or a metadata ciphertext of at most 512 bytes
after decoding. Supply a client-generated 16-byte ID when setting an
initial name. The server can assign the ID for an unnamed role. Use the
role ID's UTF-8 bytes as the metadata object ID, including for built-ins.
W05 role label blobs had no epoch or required encoding. The upgrade clears
those labels and preserves the roles and assignments. The server allows
up to 128 roles including built-ins.
Editing a custom role requires outranking it and holding every flag it
grants both before and after the change; built-in roles keep their flags.
It writes the audit action `role_changed` and sends `channel_changed`, so
clients reload roles and their own permissions.
Deleting a custom role requires clearing its assignments and replacing it
as the default first. Changing the default affects future joins.

Kick, ban, role assignment and voice moderation require the actor to outrank the target
strictly. Granting or removing a role, or choosing it as the default, also requires the
actor to outrank that role and hold every permission it grants. Assignment checks only
the roles it adds or removes: roles the member keeps pass, even ones the actor could
not grant. When upgrading from before events,
a member who could already manage a particular role can keep assigning, renaming,
editing or deleting it, or setting it as the default. The server checks both the
original and current rank and permissions. Only the event flags added by the
migration can be retained, and removing a flag ends its exemption. These grants
give no direct event permissions. The owner role comes from a singleton ownership record and can only move via
the transfer endpoint. The prior owner keeps their assigned roles.

Invite uses may be 1–1000, with lifetimes from 1 second to 30 days.
The server records who made each invite. Invites made before that have a
null `created_by` and need `manage_server` to stop. An invite works only while
its maker is the owner or a member whose roles grant `invite`: leaving, a kick
or a ban deletes their invites, and losing `invite` makes them unusable (and
unlisted) until it is granted again. Sign-in then answers `invalid_invite` and
preview 404, as for an unknown code. The invite list never
contains codes: the server stores only their hashes. The bootstrap owner
invite is never listed. The
bootstrap invite is single-use and expires after a day. For a regular
invite, supply the encrypted space key and, if the server has a name,
a separately encrypted copy of that name. Encrypt the copy using the
current space epoch even if the server's stored metadata uses an older
epoch. An invite issued before rotation can still preview the snapshot
from issuance. Kick and ban
remove membership and drop all its sessions. Unban clears the person ban
and any linked IPs. A former member can redeem a fresh invite or explicitly
join again while open joining is enabled. Use a ban to prevent rejoining. When
`ban_ip` is true, the server adds the target's IPs from active sessions.
An IPv6 address bans its whole /64, matching the rate-limit grouping.
It refuses an IP ban when no such IP is available or when a banned IPv4
address or IPv6 /64 is shared with the actor or an equal or higher-ranked
active member. The
owner retains access from a banned IP so an IP ban cannot lock them out.
The ban list returns whether each ban has linked IPs.
The server records who placed each ban and their rank at that moment. Only the
owner, the person who placed it, or someone who outranks that recorded rank can
lift it; anyone else gets 403 `role_hierarchy` and the ban and its IPs stay.
Bans from before the placer was recorded are matched to their `member_banned`
audit entry, using the placer's rank at upgrade time; when the placer is unknown
or no longer a member, anyone with `ban` can lift it, as before. The ban list's
`can_lift` says whether the caller may lift each ban; older servers omit it.

An existing member registering a new device records `device_added`. Revocation records
`device_revoked`. Both identify the device in `target_id`.

Each audit entry has `{id, actor_person_id, action, target_person_id, target_id, at}`.
A target field is null if the action has no target of that type. Use the
last returned ID as the next `after` cursor. Omit `after` to start at zero.
The optional `limit` is 1–100 and defaults to 100. The optional `action`
matches the action name exactly and applies before paging. Settings changes
record `open_joining_changed`. Open admissions record `joined` with a null
`target_id`.

## Errors

Application errors use `{"error":"code"}`. General HTTP routing and transport
errors may have an empty body. The API logs a request ID, status and reason.

| Status | Codes |
| --- | --- |
| 400 | `invalid_json`, `invalid_encoding`, `invalid_query`, `invalid_forwarded_ip`, `invalid_revocation`, `invalid_invite_limits`, `invalid_role_rank`, `invalid_role_count`, `role_id_required`, `already_owner`, `unexpected_invite`, `invite_name_required`, `invalid_member_cap`, `invalid_certificate`, `invalid_metadata`, `invalid_envelope`, `invalid_channel_flags`, `invalid_audio_kbps`, `invalid_member_limit`, `invalid_focus_time`, `invalid_order`, `invalid_dm_peer`, `not_a_voice_channel`, `invalid_message`, `invalid_keep`, `invalid_file_body`, `invalid_file_acceptance`, `invalid_event_query`, `invalid_sync_blob`, `invalid_event`, `invalid_answer`, `invalid_event_time`, `invalid_listing`, `invalid_listing_settings`, `invalid_index_url`, `invalid_index_tls_fingerprint` |
| 401 | `invalid_challenge`, `invalid_certificate`, `invalid_device_signature`, `invalid_session`, `token_expired`, `invalid_livekit_webhook` |
| 403 | `invite_required`, `invalid_invite`, `person_banned`, `ip_banned`, `device_revoked`, `device_certificate_required`, `permission_denied`, `role_hierarchy`, `cannot_grant_permission`, `built_in_role`, `owner_required`, `open_joining_disabled`, `scope_read_denied`, `recipient_read_denied`, `channel_read_denied`, `channel_write_denied`, `live_file_peer_not_in_room`, `voice_channel_full` |
| 404 | `invite_not_found`, `member_not_found`, `ban_not_found`, `role_not_found`, `category_not_found`, `channel_not_found`, `message_not_found`, `message_device_not_found`, `file_not_found`, `avatar_not_found`, `icon_not_found`, `emoji_not_found`, `pack_not_found`, `event_not_found` |
| 408 | `file_upload_timeout` |
| 409 | `owner_cannot_leave`, `already_banned`, `no_active_ip`, `role_exists`, `role_in_use`, `role_limit`, `member_cap_reached`, `device_limit`, `device_certificate_changed`, `revocation_limit`, `epoch_changed`, `space_key_unavailable`, `metadata_epoch_changed`, `category_exists`, `category_in_use`, `channel_exists`, `order_changed`, `message_exists`, `file_exists`, `file_shared`, `shared_limit`, `emoji_exists`, `emoji_limit`, `sticker_limit`, `pack_exists`, `pack_limit`, `last_pack`, `events_disabled`, `event_exists`, `event_approved`, `event_ended`, `event_limit`, `suggestion_limit`, `remind_too_soon` |
| 413 | `body_too_large` when generated after reading the body, `file_too_large`, `file_member_quota`, `file_server_quota`, `emoji_too_large` |
| 415 | `expected_json` |
| 429 | `challenge_limit`, `invite_preview_limit`, `open_join_limit` |
| 500 | `database_error`, `database_pool_timeout`, `database_busy`, `clock_error`, `randomness_error`, `invalid_stored_person`, `invalid_stored_certificate`, `invalid_stored_commitment`, `invalid_stored_device`, `invalid_stored_event`, `invalid_stored_message`, `invalid_stored_sender`, `file_storage_error`, `file_task_failed`, `emoji_storage_error`, `emoji_task_failed`, `file_expiry_overflow`, `listing_settings_encode`, `listing_settings_encrypt`, `listing_settings_decrypt`, `listing_settings_decode`, `livekit_token_failed`, `event_encoding`, `event_timeout`, `socket_io_error` |
| 502 | `livekit_unreachable`, `livekit_rooms_unavailable`, `livekit_roster_unavailable` |
| 503 | `file_upload_busy`, `voice_unavailable` |

## Keys and encrypted metadata

All routes in the table require a bearer header except invite preview.
A scope is `{kind, id}` where kind is `space` or `channel` and the ID is
the server ID or the channel ID. Epochs are positive integers, with 0
in a state response indicating that the first epoch is still pending.

| Method and path | Body | Permission and success |
| --- | --- | --- |
| GET `/v1/member-devices` | Empty | Any member. 200, `[{person, certificate}]` of current devices |
| GET `/v1/keys` | Empty | Any member. 200, `{scopes, envelopes, pending, revocations?, supports_exclusions?}`. Each revocation is `[person_id, signed_record]`, both base64url |
| POST `/v1/keys` | `{scope, epoch, device, envelope}` | A reader of the scope. 204. The recipient must also be a current device of a reader |
| POST `/v1/keys/reject` | `{scope, epoch, sender, envelope}` | A reader of the scope. 204. Deletes a matching envelope addressed to the caller's device |
| POST `/v1/keys/epochs` | `{scope, epoch, commitment, envelope, excluded?, revocations?}` | A reader of a rotating scope, the owner for the initial space epoch, or either person in a DM at any time. 201. The envelope is addressed to the caller's device |
| GET `/v1/space` | Empty | Any member. 200, `{name_ct, description_ct, icon_hash, system_messages_ct, first_text_channel}` |
| GET `/v1/space/icon` | Empty | Any member. 200, `{icon_ct}`, or `404 icon_not_found` |
| PUT `/v1/space` | `{name_ct?, description_ct?, icon_ct?, system_messages_ct?}` | `manage_server`. 204 |
| GET `/v1/members` | Empty | Any member. 200, `[{person_id, display_name_ct, avatar_hash, avatar_initialized, bio_ct, role_ids, presence, hide_offline, voice_muted?}]`. `bio_ct` is always present (null without a bio), so clients know the server stores bios. Clients keep members with `hide_offline` out of the member list while their presence is `offline`, but still use their name and picture elsewhere. `voice_muted` is present and true while a moderator mutes them |
| GET `/v1/members/{person}/avatar` | Empty | Any member. 200, `{avatar_ct}`, or `404 avatar_not_found` |
| PUT `/v1/me/profile` | `{display_name_ct?, avatar_ct?, bio_ct?, avatar_if_unset?}` | Updates the authenticated member's own profile. 204; conditional avatar publication returns 412 if already set or cleared |
| POST `/v1/invite-preview` | `{code}` | No bearer. 200, `{space_key_ct, name_ct}`. Missing, expired, spent or unpreviewable invite returns 404 |

`system_messages_ct` seals the raw text channel ID with the space key. Empty plaintext
disables notices. A null field restores the default, the first text channel by category
and channel position. `first_text_channel` is null unless the caller can read it.
An omitted PUT field preserves the choice. Older clients ignore both GET fields.

With `supports_exclusions`, scopes and wrapped keys may carry `excluded`, a
sorted set of `base64url(person_id).base64url(device_signing_key)` strings. New epochs carry forward the full
set. Adding a key authorizes a reader to advance a scope even when the rotate
flag is clear. Supply a root-signed `[person_id, record]` for each newly excluded
key. The API checks that the device was registered to that owner on this server
and that the owner currently reads the scope. Dropping previous exclusions returns
HTTP 422 `epoch_exclusions_required`. Each set and proof list is limited to 4,096
entries, with at most 64 devices per owner in a scope. The signed envelope
commitment authenticates the set (see docs/crypto.md). Protocol 3 clients create
protected epochs only on servers advertising protocol 3 or later. On older
servers they continue using ordinary epochs.

Protocol 3 servers require a client declaring protocol 3 or later at sign-in
or open join. Missing or lower values return HTTP 426 `client_update_required`.
Upgrading the server expires existing sessions once to enforce the same minimum
on reconnect and refresh. Released 0.1.8 and 0.1.9 apps see protocol 3 in server-info
and prompt for an update before signing in.

Scope state entries have `{scope, epoch, rotate, excluded?}`. Pending entries have
`{scope, epoch, device}`. Envelope entries add `sender`, `commitment`, `envelope` and optional `excluded`
to those fields. Each response exposes envelopes only for its authenticated
device and readable scopes. Pending wraps include all epochs, and include
records whose sender device is no longer current. Each new epoch is
created in one transaction with its first envelope. A client that loses
the epoch race receives `409 epoch_changed` and fetches state again.
Rejecting a matching envelope reopens its slot and prevents the sender's
person from uploading to that slot while they remain a member. A stale
rejection leaves the current envelope in place.
See [Crypto](crypto.md) for the binary envelope and metadata formats.

Space and profile PUTs update the supplied fields. Omitted fields keep their
values and explicit null clears a field. Metadata writes require the current
space epoch.
`avatar_if_unset: true` makes the entire update conditional on the avatar never
having been set or cleared. The check and write are atomic. Clients use this only
when `/v1/members` includes `avatar_initialized`, since older servers ignore unknown
fields. Every explicit avatar write, including null, marks it initialized. Existing
non-null pictures are marked initialized by migration; historical nulls cannot be
distinguished from never-set pictures because older versions kept no tombstone.

The desktop caches the selected default picture in its account's local sync state.
Only its digest goes into encrypted device sync, keeping full-size images out of
the settings blob. A fresh device obtains the matching image from a joined server;
until that source is reachable and decryptable it waits. Old accounts bootstrap
from their first successfully decrypted own picture without replacing a synced
selection. Repair fills only uninitialized memberships, after keys and device sync
are available, and never replaces an existing server picture. Missing destinations
retry every 30 seconds and after reconnect/key delivery. Explicit selection still
updates joined servers. A queued selection is discarded if device sync has since
selected a different default.

During space rotation, the API returns `409 space_key_unavailable`.
A ciphertext from a different epoch returns `409 metadata_epoch_changed`.
Server names, descriptions, channel names and topics, category names,
display names and bios allow 1024 plaintext bytes each. The desktop app caps a
bio at 8 lines and 190 characters, which always fit.

The desktop app keeps one default display name and one bio for the person,
synced between their devices in the encrypted device sync blob together with
the servers where they picked a name of their own. It publishes the default
name, sealed for each server, to every server without its own name, and the
bio to every server. A device remembers which servers still have to receive a
change made on it and sends it when such a server next loads its member list,
so servers that were offline catch up. For people from before default names,
the name used on most servers becomes the default once every server has
loaded (or when they first change a name), and servers with a different name
keep it as their own. The directory routes return a
short hash of each icon and avatar ciphertext instead of the ciphertext, so
clients fetch a picture only when its hash is new. Icon and avatar ciphertext
blobs allow 256 KiB each. The two metadata PUT routes that accept images
allow 356 KiB JSON bodies to account for base64url and accompanying fields.
Avatars and icons are PNG, JPEG or animated GIF on the wire, so older clients show
them (an animated one plays) and no field or limit changes. The desktop picker also
takes WebP and animated WebP: it crops every frame and sends a still as PNG, an
animation as a looping GIF of at most 256×256 and 100 frames, with frames dropped
(keeping the total time) and then the size reduced until it fits the 256 KiB limit.
Clients show the first frame and play the animation only while it is hovered.
Invite preview permits 32 lookups per IPv4 address or IPv6 /64 and
256 server-wide in 60 seconds. Invalid codes that pass base64url parsing also count.

### Server emoji and stickers

| Method and path | Body | Permission and success |
| --- | --- | --- |
| GET `/v1/emoji` | Empty | Any member. 200, `{max_emoji, max_stickers, max_bytes, max_packs, packs: [{id, name_ct}], items: [{id, kind, owner, size, image_hash, name_ct, pack_id}]}`; packs in display order, items oldest first |
| GET `/v1/emoji/{id}/image` | Empty | Any member. 200, `{image_ct}`, or `404 emoji_not_found` |
| POST `/v1/emoji` | `{id, kind, name_ct, image_ct, pack_id?}` | `manage_emoji`. 201. `pack_id` defaults to the first pack |
| PUT `/v1/emoji/{id}/pack` | `{pack_id}` | `manage_emoji`. 204 |
| POST `/v1/emoji/packs` | `{id, name_ct}` | `manage_emoji`. 201, added last |
| PUT `/v1/emoji/packs/{id}/name` | `{name_ct}` | `manage_emoji`. 204 |
| PUT `/v1/emoji/packs/order` | `{ids}` | `manage_emoji`. 204. Lists every pack once |
| DELETE `/v1/emoji/packs/{id}` | Empty | `manage_emoji`. 204. Deletes the pack's emoji and stickers too |
| PUT `/v1/emoji/{id}/name` | `{name_ct}` | `manage_emoji`. 204 |
| DELETE `/v1/emoji/{id}` | Empty | `manage_emoji`. 204 |

`kind` is `emoji` or `sticker`. The ID is a random 32-byte client-generated
value; reusing a current ID returns `409 emoji_exists`. Names and pictures are
space metadata (types 10 and 11 in [Crypto](crypto.md#metadata)) under the
current space epoch, so the server knows only the ID, kind, ciphertext size,
picture hash and the person who added it. Clients upload PNG or animated GIF
pictures and names of 2–32 lowercase letters, digits and `_`; the server cannot
check either. The server allows `RUSTTALK_EMOJI_MAX` emoji (default 100) and
uses the effective sticker limit from `GET /v1/settings/files`, capped by
`RUSTTALK_STICKER_MAX` (default 50). It returns `409 emoji_limit` or
`409 sticker_limit` past them. A picture ciphertext larger than
`RUSTTALK_EMOJI_MAX_BYTES` (default and maximum 262,144) returns
`413 emoji_too_large`. POST allows a 356 KiB JSON body. Pictures are stored as
files beside the database. Changes record `emoji_added`, `emoji_renamed` and
`emoji_deleted` audit entries with the emoji ID as `target_id`, and send
`channel_changed`.

Every emoji and sticker is in one pack. The migration creates the Default pack
(the ID of 32 zero bytes) with an empty name, since the server cannot encrypt
one; apps show their own "Default" until it is renamed. Pack names are space
metadata (type 14) with the same epoch rules as emoji names. The server allows
`RUSTTALK_EMOJI_PACKS_MAX` packs (default 20) and returns `409 pack_limit`
past it, `409 pack_exists` for a reused ID, `404 pack_not_found` for an unknown
pack, `409 order_changed` when an order lists other packs than exist,
`400 invalid_order` for a repeated ID, and `409 last_pack` when deleting the
only pack. Pack changes record `emoji_pack_changed` audit entries and send
`channel_changed`.

Messages carry a server emoji as the token `<:name:id>` inside the encrypted
text, with the base64url ID. Clients show the picture when this server has
that ID and `:name:` otherwise. A sticker is its own message, see
[the plaintext format](crypto.md#messages).

An emoji or sticker of another server (or in a DM) cannot be read from the
server that holds it, so the sender's app attaches the picture to the message
as a small stored file on the server the message goes to, without a thumbnail.
The file's media type is `application/vnd.rusttalk.sticker` or
`application/vnd.rusttalk.emoji` and its name is `name.gif`, `name.png` or
`name.jpg` with a valid emoji name. A sticker message has the text `:name:` and
the file; an emoji message keeps its `<:name:id>` tokens and carries one file
per emoji, matched to the token by name. The file counts toward that server's
file storage like any file. Receivers accept it only when the type, a name
that fits, a size of at most 262,144 bytes minus the encryption framing, no
thumbnail, a PNG, JPEG or GIF of at most 100 frames and 32 MiB decoded, and at
most 128 pixels on a side (emoji) or 320 (stickers) all hold; anything else
stays an ordinary file. Apps that do not know the types show a small file.

## Channels and categories

All routes require a bearer header. Channel kind is `text`, `voice`, `page`, `forum`, or `dm`.
A `forum` channel also works exactly like a text channel on the server, with the
usual defaults. Clients group its messages: a message without `reply_to` is a post
whose first line is its title, and a message whose `reply_to` leads back to a post
is a reply in that post's thread. A post with a poll is sent as the poll, then the
post's text with `reply_to` set to that poll by the same author and the encrypted `quoted_author` hint set to 32 zero bytes. Ordinary replies (including older, unmarked pairs) stay replies; a poll nobody's text
carries is a post of its own. Up and down votes on a post are poll votes whose `poll`
is the post's ID, option 0 up and 1 down, so they change and are taken back like
any vote. The grouping and the votes are inside the encrypted messages, so the server
does not know which messages belong together. The 10,000-message limit applies to
the whole forum.
A `page` channel works exactly like a text channel on the server (same messages,
keys and flags); clients show only its newest message, as one Markdown page, and
saving a change posts the whole page as a new message. A new page channel gives
read and write (3) to Owner, Admin and Moderator and read (1) to Member, so only
people with write can change the page. Older clients show it as a text channel. The page's pictures are
that message's stored files (at most four, like any message). Saving a change
sends the references of the pictures it keeps again in the new message, so their
blobs are not uploaded twice. A version saved with the same link also keeps its
link preview. The message post of a page version adds `keep`, the IDs of every
stored file it references (files and thumbnails, at most 8, no repeats). The API
keeps only those that are unexpired files stored in that page channel and leaves
the others out, so a page whose picture is gone still saves; `keep` on any other
channel, a malformed ID, a repeat or more than 8 IDs is `400 invalid_keep`. An
editor can thus keep any 8 live files of the page channel; they still count toward
their uploader's quota. Files the page's newest version keeps never expire. When a newer version (or deleting the
newest) leaves a file out, it expires with the server's file retention from its
upload time again, like any other file. A version saved without `keep`, as older
clients do, keeps nothing. When the editor who saved a version leaves, is kicked
or banned, or revokes the device that saved it, that version no longer keeps
files. Older servers ignore `keep`, and the pictures expire as before.
Several versions can thus reference one blob: when deleting a page version, the
client skips the files another loaded version still references. Older clients,
which show a page as a text channel, do not; a moderator's delete still cannot
remove another member's file (see Files). Link previews on a page work exactly as
in chat.
Channel and category IDs are client-generated random 32-byte values.
`position` is an integer used for ordering with ID as the tiebreaker.
Text and voice channels each have one category. Channel kind is fixed at creation.
A new channel goes last in its category and a new category goes last; the
`position` in a create body is ignored. The order body must list every
category and every text or voice channel exactly once, in the new order and
any category; the body may be up to 64 KiB. A repeated ID returns
`400 invalid_order`; a missing or unknown ID returns `409 order_changed`.

| Method and path | Body | Permission and success |
| --- | --- | --- |
| GET `/v1/catalog` | Empty | Any member. 200, `{channels, categories}` filtered to channels they can read and their categories |
| GET `/v1/channels` | Empty | `manage_channels`. 200, channel list |
| POST `/v1/channels` | `{id, kind, category_id, position, name_ct, topic_ct?, commitment, envelope}` | `manage_channels`. 201. Envelope initializes epoch 1 for the caller |
| PUT `/v1/channels/{id}` | `{category_id, position, name_ct, topic_ct?, audio_kbps?, member_limit?}` | `manage_channels`. 204. `audio_kbps` (voice channels only, 8 to 128, default 64) is the Opus bitrate everyone publishing in the call uses; when absent it stays as it was. The catalog and channel list return it as `audio_kbps` for voice channels, and `POST /v1/voice/{channel}/token` returns it too. A change sends `channel_changed`, and clients in the call republish their audio at the new bitrate. `member_limit` (voice channels only, 0 to 99, 0 = no limit, default 0) is the most people in the call at once; when absent it stays as it was. The catalog and channel list return it as `member_limit` for voice channels that have one |
| DELETE `/v1/channels/{id}` | Empty | `manage_channels`. 204 |
| PUT `/v1/channels/{id}/permissions/{role}` | `{flags}` | `manage_channels` and a lower-ranked target role. The Owner can also change the Owner role. 204 |
| PUT `/v1/channels/{id}/focus` | `{seconds}` | `start_focus` and read access to a voice channel (other channels it can read: `400 not_a_voice_channel`). 204. Turns focus mode on until `seconds` (1 to 86,400) from now, or off with 0; anything else is `400 invalid_focus_time`. The catalog and channel list return the end as `focus_until` (Unix seconds) while it is in the future. A change sends `channel_changed`; the end itself sends nothing, clients compare it with their clock |
| GET `/v1/categories` | Empty | `manage_channels`. 200, category list |
| POST `/v1/categories` | `{id, position, name_ct}` | `manage_channels`. 201 |
| PUT `/v1/categories/{id}` | `{position, name_ct}` | `manage_channels`. 204 |
| DELETE `/v1/categories/{id}` | Empty | `manage_channels`. 204 if empty, 409 if in use |
| PUT `/v1/channels/order` | `{categories: [{id, channels: [id]}]}` | `manage_channels`. 204. Sets every category's and text or voice channel's `position` and `category_id` in one transaction, audited as `channels_reordered`, then sends `channel_changed` |

Channel responses have `{id, kind, category_id, position, name_ct, topic_ct, flags}`.
A category has `{id, position, name_ct}`. Each channel's flags are
`[{role_id, flags}]` with read (1), write (2), speak (4), and video (8).
Flags accumulate across assigned roles, including the owner role while
held. The owner always reads every text and voice channel, so the Owner role's read flag
never removes the owner's access. A zero mask removes a role's entry.
Write, speak and video each require read in the same mask. An invalid
mask returns `400 invalid_channel_flags`. Owner, Admin and Member start
with all four flags on new channels. If a custom-role creator would
otherwise lack read access, its highest-ranked assigned role with
`manage_channels` also starts with all four flags. Losing the read flag
through any assignment or permission change requests a channel rotation.
Channel names and topics use space keys, while channel keys protect
the channel's content.

## Server events

Events are optional: an app that gets 404 from `GET /v1/calendar` hides them.
They are off until someone with `manage_server` turns them on.

| Method and path | Body | Permission and success |
| --- | --- | --- |
| GET `/v1/calendar` | Empty | Any member. 200, `{enabled, events}`. Approved events, plus waiting ones for their author and for `manage_events`. Empty while off |
| PUT `/v1/settings/calendar` | `{enabled}` | `manage_server`. 204 |
| POST `/v1/calendar` | `{id, epoch, remove_at, envelope}` (body up to 4,840 bytes) | `create_events` (approved at once) or `suggest_events` (waits). 201, the event |
| POST `/v1/calendar/{id}/approve` | Empty | `manage_events`. 204 |
| DELETE `/v1/calendar/{id}` | Empty | The author or `manage_events`. 204, also while off |
| PUT `/v1/calendar/{id}/answer` | `{epoch, envelope}` | Any member, for an approved event that hasn't ended. 204. Replaces their earlier answer |
| POST `/v1/calendar/{id}/remind` | Empty | The author or `manage_events`, for an approved event that hasn't ended, once an hour. 204, sets `reminded_at` |

An event is `{id, author, author_device, certificate, approved, epoch, remove_at, envelope, answers, reminded_at?}`, each answer `{person, device, certificate, epoch, envelope}`
(see `docs/crypto.md`, Server events). `id` is a client-generated random 32-byte
value from the same namespace as channel IDs and is never reused. `epoch` must
be the current space epoch. `remove_at` is the end plus two hours; the end must
be in the future and at most 366 days away. A server holds 50 events and a
person can have 5 suggestions waiting. Due events are left out at once and
deleted within 30 seconds. Every change sends `channel_changed`, which older
apps understand; apps that haven't answered show a newer `reminded_at` once as a
desktop notification. Changes other than a new suggestion, its withdrawal, answers, reminders and
expiry are audited.

## Direct messages

| Method and path | Body | Permission and success |
| --- | --- | --- |
| POST `/v1/dms` | `{id, peer, commitment, envelope}` | Any member. 201, `{id}`. Returns 200 with the existing ID if the pair already has a DM |

`peer` is a different current person. Either person can create the DM, using a
random ID and an epoch 1 envelope addressed to their device. The normal channel
key flow wraps keys for both people's devices, including their later devices.
Both people can read and write regardless of role. DMs are visible only to the
two participants, including when an Owner, Admin or Moderator requests access.
Only its author deletes a DM message or file: `delete_messages` does not apply
in DMs, so the other person gets `403 permission_denied`.

The catalog includes DMs with `kind: "dm"`, `peer` set to the other person ID,
`category_id`, `name_ct` and `topic_ct` set to null, `position: 0`, and
empty `flags`. Clients display the peer's decrypted display name or person
fingerprint. Channel management routes cover text and voice channels.
When either person leaves the server through membership removal, the API
closes the pair's DM and deletes its history and envelopes. They can create a
new DM if the former member rejoins. Copies already held by either person can
remain on their devices.

The app keeps one DM per person over every server both people share. Each
server holds an ordinary DM channel, its copy. The app creates the copy on a
shared server when it first sends there. It posts each message, with the same
random message ID, to every shared server that is online. Copies are encrypted
separately for each server (see [Crypto](crypto.md#direct-messages-on-several-servers)).
Readers load every copy, merge records by person and message ID, and order
them by the earliest ordinary receipt time (or earliest restored time if all copies are historical), keeping server order for equal times.
Only copies that open count: a record that fails to verify or decrypt stays a
placeholder for its own server. It doesn't change a readable copy's time or
servers, doesn't notify or mark the DM unread, and is replaced whole once a
readable copy arrives. A DM shows records from its two people only.

On connection, reconnection, membership or DM changes, and hourly while online, the author’s app compares the newest 256 records of
each shared DM copy in pages of 32. One scan runs at a time, with at least
250 ms between network commands. It opens its own missing records, oldest
first, and posts them under the original IDs to the pinned destination DM,
creating the channel there if needed. The optional DM-only POST field
`backfill_at` preserves the source timestamp, capped at the destination’s
current time. Servers advertise support with `dm_backfill: true` in server info.
Historical copies have `backfilled: true` in POST, page and lookup responses and
get a new local sequence number. Pagination and retention still use local
insertion order. A full DM refuses
a historical insert with `409 history_full` so live history is preserved.
After interruption the next scan uses the IDs already stored as its checkpoint.
Deleted or pruned IDs remain reserved. Epoch links and live transfers stay
local to their channel. The background scan skips records with attachments
to keep file transfers off the connection loop. Successful and permanently
refused IDs are remembered per route for the session. A full destination page
window limits repair to records newer than its oldest entry. Custom emoji and
stickers use the destination catalog. Ambiguous sticker names fall back to text
unless the selected sticker ID is present.

Before sending, the app pins each DM channel to the person the catalog first
listed it with, in `dm_pins/<server ID>.json` on the device, with the anchor:
the newest epoch it knows only the two people hold, and its commitment. A DM
the app creates is pinned before its key is wrapped, so epoch 1 is the anchor.
Later epochs are trusted through key links: before starting a DM's next
epoch, the app sends a key link message in the current epoch with the next
epoch and its commitment (see [Crypto](crypto.md#direct-messages-on-several-servers)).
Apps hide key links; they don't notify or mark anything unread. When the
current epoch doesn't chain to the anchor, the app reads the DM's recent
history for its link, and otherwise sends a link and starts the next epoch
itself, which either person may do at any time, wrapped only to the two
people's devices. A channel the server later lists with someone else is
skipped, with a warning. If the pin file can't be read, the app leaves it as
it is and pauses that server's DMs, also new ones, with a warning, instead of
trusting the next catalog. A message, friend message or live-file offer goes
to a copy only when its current epoch is trusted and its key came from a
device of one of the two people. Otherwise the app refuses with "Not sent over
<server>: someone else could read this DM there." If a send or live-file offer
meets `409` because the other person just re-keyed, the app takes the new key
and tries once more; if that fails too, a message goes back to the composer.
DM epoch commitments name the two people (see
[Crypto](crypto.md#channel-and-space-keys)), so DMs need both people on this
version. No API or protocol change is needed. If the pins can't be saved, the
app pauses that server's DMs with a warning. A server without DM re-keying refuses the new epoch, so its copies
stay unused.

An edit goes to the servers holding the original. A `message_deleted` event
removes only that server's copy, because a server can't show who deleted a
record and retention prunes each server separately. The message disappears
once no server that showed it holds it. People can delete only a readable
copy of their own message, since a copy that doesn't open proves nothing about
its author. Their app deletes it, and each copy's stored files, on every
online server holding it, and keeps the delete in the synced setting
`dm_deleted`: per message ID (base64url), the servers that may still hold a
copy (`left`) and those whose copy is gone (`done`). Devices merge these as
sets across every sync copy, so a delete can't be lost when two devices publish
to different servers. Copies are kept by server and DM channel. A copy counts
as gone only once a fresh request for it in that DM channel returns 404, not on
the server's delete event alone. Their apps on every
device hide that message and delete any readable copy that shows up later,
with its stored files, also after a restart. Each device keeps every delete
that still waits for a server, and the newest 1,000 others, in
`dm_deleted.json`; the sync copy holds the newest that fit in 64 KiB, resolved
ones dropped first. A server can still claim a copy is gone and keep it; that
copy is deleted again whenever it shows up, until its delete is among the
oldest dropped. A copy of the other person's deleted message on a server that
was offline shows again until that person's app deletes it there.

After reconnecting, a DM counts as unread only when its newest record opens,
comes from the pinned peer and isn't a key link. Unread marks and mention
counts are kept per server and channel, so a channel on another server under
the same ID can't change a DM's mark. Each copy keeps its own attachment
references, and downloads switch to the next copy's when a server drops the
shown one.

Older pages load from each copy separately, and the view asks each copy on
its own, so a slow server doesn't hold up the others. A page that fails stops
only that copy's paging until its server reconnects; when its cursor is gone
(404), that copy loads from its newest page again. Without a shared server, the app keeps
showing what it loaded and disables sending. The API still
sees each copy as a separate DM, so older apps show one DM per server.

### Friends

Friendship is only between the two apps; the API has no friend routes. A
request or answer is a friend message (see [Crypto](crypto.md#messages)) in
the DM, sent like any DM message: one random ID, posted to every shared server
that is online. So a request reaches only people the sender shares a server
with, like a DM. The kinds are:

| Kind | Sender's list | Receiver's list |
| --- | --- | --- |
| Request | Outgoing | Incoming. If the receiver had asked too, both become friends. If they were friends already, the receiver answers with Accept |
| Accept | Friend | Friend, if it had asked |
| Decline | None | None, if it had asked |
| Cancel | None | None, if it was asked |
| Remove | None | None, if they were friends |

Blocking someone sends Remove, Cancel or Decline as fits, so it looks like one
of those, and then ignores their friend messages. Each list entry keeps the
signed send time of the newest friend message handled from that person (from
the message itself, not the server) and the IDs of the last eight. A copy on another server, a reload, or an older message
loaded later (pages load newest first) changes nothing. Applying the same
action twice changes nothing either. The app reads each DM's newest page when
it connects, so requests sent while it was away show up; one older than that
page shows when the chat is scrolled back.

The list itself is in the person's sync blob under `contacts`: per hex person
ID, `{relation, name, changed, since, seen}`. `relation` is `none`,
`outgoing`, `incoming`, `friend` or `blocked`; `name` is the last name seen;
`changed` is when the relation last changed, in Unix milliseconds. When two
devices changed the same person, the later `changed` wins, whichever blob is
newer (see [Sync](#sync)).

"Allow DMs from: Friends only" is a synced setting (`friends_only`) that the
receiving app enforces: DMs from people who aren't friends (or asked by this
person) go under "Message requests", without notifications or unread marks.
Blocked people's DMs are hidden and never notify. The servers still deliver
these messages; refusing them there would mean telling the server who is a
friend.

### Pokes

A poke is a short note that pops up for someone. It is a DM message with the
poke flag (see [Crypto](crypto.md#messages)), sent like any DM message: one
random ID, posted to every shared server that is online. So a poke reaches only
people the sender shares a server with, and only through a DM copy that just the
two people can read. The API has no poke routes.

Apps keep pokes out of the DM's chat and list them on a Pokes page instead,
received and sent, with the server they came over. A live poke from the other
person shows an in-app pop-up and a desktop notification, following the DM's
notification settings with that person: none when that DM is muted or set to
nothing, and a poke counts as a mention for "Mentions only". Pokes don't mark
the DM unread. The app reads each DM's newest page when it connects, so pokes
sent while it was away show on the Pokes page, unread when under a day old,
without a notification. Copies on other servers and reloads count once, by
message ID.

The receiving app drops pokes from blocked people and, with "Allow DMs from:
Friends only", from people who aren't friends. It also takes at most three
pokes from one person per minute of server time and drops the rest. The history
(the newest 200) stays on the device in `pokes.json`; it is not synced.
Apps before pokes show one as a message they can't open.

### Voice requests

A voice request asks someone to join the voice channel the sender is in. It
and its answer are DM messages of kind 4 (see [Crypto](crypto.md#messages)),
each with a random ID. They go only over the channel's own server, in the DM
with that person, and need protocol 2 or later there. The API has no routes
for them.

Apps keep both out of the DM's chat, and they don't mark it unread. A live
request from the other person rings for 30 seconds with a pop-up and a silent
desktop notification, following that DM's notification settings like a poke.
The receiving app drops a request from a blocked person, from a non-friend
with "Friends only", for a channel this person can't open or is already in,
one outside the 30-second window starting at the authenticated sender timestamp, a
repeated one, and more than three from one person per minute. "Mute for 15
min" drops that person's requests and rings on that device until the app
closes or the time is up, and sends nothing.

The asking app takes the first answer from the person it asked that names its
request's ID, and shows "no answer" after 35 seconds. An answer also closes
the pop-up on the answering person's other devices. Verified DM history can resolve
an active request. Historical invitations and locally restored pages never ring.

So that the others in the call hear it, the asking app sends each other device
in the room a LiveKit data packet with topic `rusttalk.ring`, reliable and
sealed like [remote control](#remote-control) messages with HKDF info
`rusttalk-ring-v1` in place of `rusttalk-control-v1`, up to 128 bytes on the
wire. The sealed JSON integer records Unix milliseconds. Receivers accept it only
within 30 seconds of that timestamp. Each process remembers up to 256 fresh packets
across room reconnects and drops duplicates and overflow. Receivers play it at
most once every 3 seconds, and not from blocked or muted people. A sender
without write permission in the channel can't send data, so then only the two
of them hear the ring.
Apps before voice requests show both messages as ones they can't open, notify
for them as new messages and ignore the data packet.

### Polls

A poll is a message in a channel or DM whose format 3 kind byte says poll (see
[Crypto](crypto.md#messages)): its question, 2–10 options, whether more than
one answer may be picked, and an optional end time. Votes and early ends are
small messages of their own that name the poll's message ID inside the
encrypted body, so the server sees only ciphertext, sizes and times. The API
has no poll routes.

Apps don't show votes or ends as chat lines; they count them on the poll's
card, and they don't mark a chat unread or notify. Each person's vote with the
highest sender clock (then server time, then record ID) counts, and replaces
their earlier ones; an empty vote takes it back. Readers ignore a vote with an
option index the poll doesn't have, more than one choice on a single-answer
poll, or a server time after the poll closed. A poll closes at its end time, or
at the server time of the first end from its creator; ends from anyone else are
ignored. Apps before polls show a poll, vote or end as a message they can't
open. Polls need format 3, so they can't be sent to a server below protocol
version 2.

### Reactions

A reaction is an emoji under a message: a Unicode emoji, or in a channel one
of the server's own emoji. It travels as a poll vote (see
[Crypto](crypto.md#messages)) whose poll ID is the first 16 bytes of the
message's ID followed by the emoji's 16-byte key, with one choice (index 0)
to add it or none to take it back. So the server sees only ciphertext, there
are no reaction routes, and apps with polls but without reactions count it
under a poll that doesn't exist: it never shows, marks a chat unread or
notifies there. Apps before polls show it as a message they can't open.

The emoji's key is the first 16 bytes of SHA-256 over
`"rusttalk-reaction-v1"`, then `0` and the UTF-8 emoji with U+FE0F removed,
or `1` and the server emoji's 32-byte ID. Apps show a reaction only under a
readable message they have loaded whose ID starts with those 16 bytes, and
only for an emoji they know: the Unicode emoji in their picker or an emoji of
the message's server. Each person's reaction with the highest sender clock
(then server time, then record ID) per message and emoji counts, like a
vote. A message shows at most 20 distinct emoji, the first to arrive by
server time. Reactions don't mark a chat unread or notify.

## Messages

These routes require a bearer header and apply to text channels, voice channels and DMs.
A voice channel's chat follows the same read and write flags, limits and events
as a text channel. Its messages use the channel key epoch that call media keys
also derive from; joining or leaving the call does not change the epoch.
IDs are random 32-byte client-generated values. A page is newest first by
insertion order. Pass the last record ID as `before` for the next page. If
that cursor was pruned or deleted, the API returns `404 message_not_found`
and the client can restart from the newest page.

| Method and path | Body or query | Permission and success |
| --- | --- | --- |
| GET `/v1/channels/{channel}/messages?before={id}&limit={n}` | Optional cursor and limit, 1–100, default 50 | `read`. 200, array of message records |
| POST `/v1/channels/{channel}/messages` | `{id, epoch, envelope}`, and a page version's optional `keep` (file IDs) | `write`. 201, the stored message record |
| GET `/v1/channels/{channel}/messages/{id}` | Empty | `read`. 200, one message record |
| DELETE `/v1/channels/{channel}/messages/{id}` | Empty | The author, or in a non-DM channel a reader with `delete_messages`. 204, else `403 permission_denied` |
| GET `/v1/channels/{channel}/message-devices/{device}/{person}` | Empty | `read`. 200, the public device certificate as a base64url JSON string, when the device authored a retained message here |

A message record has `{id, epoch, person, sender_device, at, size, envelope}`.
`at` is the server's Unix timestamp. `size` is the binary envelope length.
`envelope` carries a one-byte version, 24-byte nonce, two-byte big-endian
ciphertext length, ciphertext including its authentication tag, and 64-byte
device signature. Protocol 2 accepts versions `2` and `3`. New apps send v3 on
protocol 2 and later servers for all messages, and v2 on protocol 1 servers. Older apps
reject v3 envelopes as unreadable and prompt for an update on protocol 2
servers. The envelope is at most 10,752 bytes
(see [the plaintext format](crypto.md#messages)). The POST limit is
14,848 bytes of JSON: the unpadded base64url envelope plus 512 bytes. The API verifies the
envelope against the authenticated person and device and requires the current
channel epoch. Rotation or a stale epoch returns `409 epoch_changed`, a
duplicate retained ID returns `409 message_exists`, and malformed framing or
signature returns `400 invalid_message`. IDs stay reserved after a delete or retention drop, and reusing one also returns
`409 message_exists`. Access failures use
`403 channel_read_denied` or `403 channel_write_denied`.

Mentions are part of the encrypted text. `<@person_id>` uses a person's
base64url ID, `<@&role_id>` uses the role ID, and `@everyone` and `@here`
target every recipient in a server channel. Clients resolve display names
and role names locally. In a DM only person mentions are active.

Role-wide and channel-wide mentions require the **Mention everyone**
permission, granted to Owner and Admin by default. The sending client
refuses such a message without the permission. Receiving clients check
the authenticated sender's current roles before highlighting or notifying
for those mentions, and leave them as ordinary text if the sender or
their permissions are unknown. The API stores and authorizes the role
flag but cannot inspect ciphertext or enforce the mention rule. A modified
client can send the tokens without permission. Changes to role assignments
or permissions can change how clients display retained messages.

The API keeps the newest 10,000 records per channel. Explicit deletes remove
the ciphertext and add a `message_deleted` audit entry with actor, message ID
and time. DM audit entries are visible to participants with audit access. When the DM
closes, its entries become visible to everyone with audit access.
Retention drops also emit a delete event.

An edit is a new record with an encrypted `edit_of` reference. Clients
verify the original's signature, channel and author before accepting the edit.
Multiple edits can refer directly to the same original. An edit's reply-to
field, which apps ignore on edits, carries its revision: 24 zero bytes, then
a big-endian u64. The app sets it to its clock in milliseconds, or one above
the revision it shows if that is higher. The edit with the highest revision
wins, then the higher edit ID. An edit without a revision, from an older app,
loses to one with a revision and is ordered by server time against others
without one. Edit state is kept per person and original ID. New replies include
the quoted person ID inside the encrypted body. It is a display hint supplied by
the replier. When the original is unavailable the preview can name that person.
A unique locally verified sender takes precedence. When multiple senders share
an ID, only a matching sender's message supplies the snippet. Legacy replies
with an ambiguous ID show no snippet. If retention or a
delete removes the original, clients suppress edits that still reference that
ID. Deletes apply per stored record, because the API cannot see which records
are edits. When a client deletes a message, including a moderator delete, it
also deletes every edit of it that it has opened or sent.

## Voice

Desktop phone bridges use the encrypted `rusttalk.phone-consent` participant
attribute. Its plaintext is `{revision, hosted: track_sid_or_null, allowed:
[{host, session, track}], withdrawn: [{host, session, track}]}`. Positive
revisions increase for each publication attempt within a room and are sealed
along with grants and withdrawals. The channel's video-watch-derived key is
used with the distinct `rusttalk-phone-consent-v2` AAD label, epoch (big-endian
i64), sender identity, a zero separator, and sender session ID. The wire form
is `epoch.base64url(ciphertext)`, capped at 60 KiB when read. Receivers retain
the highest authenticated revision and its state per device/session for the
room's lifetime, across signaling reconnects and key refresh. Lower revisions,
conflicting equal revisions, invalid ciphertext and v1/legacy states grant
nothing. Hosted tracks must exist as shared-audio publications in that session.
Consent grants only that device session's microphone to that exact call;
neither Listen nor Watch grants export.

Withdrawal immediately pauses channel microphone PCM, independently of the
person's mute/deafen/PTT settings. Publishing withdrawal is not an acknowledgement.
The bridge filters its mixer and clears queued withdrawn audio from a single
consent snapshot before publishing `rusttalk.phone-withdrawal-ack`. Its sealed
plaintext is a list of `{member, session, revision, call}` using the separate
`rusttalk-phone-withdrawal-ack-v2` AAD label and the bridge's identity/session.
The sender resumes only after an acknowledgement for its device/session, call
and withdrawal revision (or newer), or the matching bridge publication/session
has ended while the room is connected. Failed signaling keeps the microphone
paused and shows pending/failure; retries do not bypass the interlock. Leaving
the room clears the interlock. A directly dialed local phone call still respects
normal microphone controls and is independent of this channel transmission pause.
Clients withdraw consent on reconnect and end hosted calls on reconnect,
loss of sharing permission, or loss of publication. Older clients cannot grant
consent and their sound is never exported by a conforming bridge.

These routes require a bearer header and channel `read` access. They apply
to voice channels only.

| Method and path | Body | Success |
| --- | --- | --- |
| POST `/v1/voice/{channel}/token` | Empty | 200, `{token, audio_kbps}`. LiveKit JWT valid for ten minutes for this room, and the channel's Opus bitrate in kbps. `403 voice_channel_full` when the channel has a member limit and that many other people are already in the call; people with `manage_channels` (and the owner) are not limited, and a person already in the call with another device takes no extra place |
| GET `/v1/voice/{channel}/participants` | Empty | 200, `{people, unknown}`: sorted unique person IDs and a count of unmatched devices |
| DELETE `/v1/voice/{channel}/participants/{person}` | Empty | `move_members`. 204, disconnects their devices from the room |
| POST `/v1/voice/{channel}/participants/{person}/move` | `{to}` | `move_members` and `read` on `to`. 204, disconnects their devices from the room and sends each removed device `voice_moved` |

A voice mute is server-wide and stored on the member, so it holds across
channels and rejoins until lifted. Leaving or being removed from the server
deletes it with the membership. While muted, tokens and access enforcement
leave out the `microphone` and `screen_share_audio` sources, so LiveKit refuses
and drops those publications: the person is fully silent, as shared audio could
otherwise carry the microphone. Enforcement sets each device's grants to what
its current flags and mute allow, in both directions, so lifting a mute gives
the sources back to devices still in the room without a rejoin. A single
worker applies it, one check at a time, deciding each device from the database
right before changing it, so an older decision cannot land after a newer
change. Pending checks coalesce: a person whose mute changed, then a full pass,
then single rooms, each queued at most once, so a burst of joins in one room
cannot hold up a mute elsewhere. The API enforces on
startup, on every LiveKit join notice (whatever the token allowed), after each
mute change. A check that fails is retried after 5 seconds while other checks
go on, and a worker that stops unexpectedly is restarted with a full pass; muting again
reapplies it without a second audit entry. Clients follow LiveKit's own
permission change: they publish the microphone again when it is allowed, even
if their API connection missed the lift.

Removing and moving fail with `404 not_in_voice` when none of the person's
devices is in the room, and a move with `403 voice_move_denied` when the person
may not read the target channel. Permission, rank, membership and channel
access are checked again after the LiveKit roster lookup. The audit entry is
written, for the acting member even if their session ended meanwhile, only after
LiveKit confirmed removing at least one device; a device that had already left
does not count, and if all had, the request returns `404 not_in_voice`. If
removing any device fails, the request returns `502 livekit_rejected` (or
`livekit_unreachable`).
A removal is not a kick: they may rejoin. Clients act on `voice_moved` only
for their own call in `from`, or just after LiveKit removed them from it, and
only when `to` is a voice channel they can read. A malicious server could still
remove a device and send a made-up move within that window. This is an accepted
risk: the server already controls membership and can disconnect anyone, a move
only goes to a channel the person may read, media stays end-to-end encrypted to
holders of that channel's key, and the client says "Moved to <channel> by a
moderator". The audit actions are
`voice_muted` and `voice_unmuted`, and `voice_removed` and `voice_moved` with
the source or target channel as `target_id`.

The room name is the encoded channel ID. The token's identity is
base64url of SHA-256 over `"rusttalk-voice-device-v1" || server_id ||
device_signing_public_key`, with each ID as 32 raw bytes. It carries
no display name or metadata. It permits subscription. Data publication
requires channel `write`. Clients consume byte streams only through the
independently authenticated live file protocol, and data packets only
on the remote control and terminal topics (see below), and drop other data
events.
Track publication requires
the relevant flag from at least one assigned role. The `speak` grant
allows `microphone` and `screen_share_audio`. The independent `video`
grant allows `camera` and `screen_share`.

Each shared application is an independent `screen_share_audio` track
with the same frame encryption as the microphone. Its LiveKit track name
is `<epoch>.<base64url>`: the decimal channel-key epoch at publication,
a dot, then unpadded base64url of the 24-byte random nonce followed by the
XChaCha20-Poly1305 ciphertext and tag. The plaintext is the app's name
limited to 128 UTF-8 bytes, cut at a character boundary. The AEAD key is
32 bytes of HKDF-SHA256 from that epoch's room media key, with no salt
and info `rusttalk-track-name-v1`. The associated data is that literal
followed by the epoch as a big-endian i64. Receivers decrypt with the
media key of the stated epoch, which they keep after rotations, and
display “Shared audio” if they lack that epoch or the name cannot be
authenticated or decoded. The name length is not hidden. The SFU can
still move a valid label to another track in the same room and epoch,
as it can move frame-encrypted tracks themselves.
Volume and mute are local and keyed by track ID.

A screen or window is published as `screen_share` with one VP9 layer and
the same epoch-bound LiveKit GCM frame encryption as audio. Screen labels
use the app audio wire format with the distinct HKDF info and associated
data prefix `rusttalk-screen-track-name-v1`. Receivers display “Shared
screen” if label decryption fails. The desktop portal supplies no title,
so Linux uses “Shared screen” or “Shared window” based on the source kind.
A camera is published as `camera` with one VP9 layer and the same frame
encryption. (With the LiveKit SDK's libwebrtc the top VP8 simulcast and VP9
SVC layers encode at only a few frames per second; a 60 fps screen keeps its
frame rate and gives up resolution first.) Its LiveKit track name is the fixed string `camera`, so the
device name never leaves the sender; receivers show “Camera”. Clients
offer 480p, 720p or 1080p at 15 or 30 fps and open the camera's closest
mode. App audio remains a separate publication. Clients join with automatic
subscription disabled, subscribe to encrypted audio when announced, and
subscribe to each encrypted video track only while the viewer chooses it:
a watched screen, or a camera while its person's tile is on screen and
the viewer has not turned that person's video off.

### Remote control

A viewer of a shared screen can ask its sharer for control of the mouse and
keyboard. The sharer must allow each request, only one person asks or
controls at a time, and nothing is ever accepted automatically. Messages are
LiveKit data packets with topic `rusttalk.control`, sent with
`destination_identities` set to the one other device. Each payload is the
channel-key epoch as a big-endian i64, then a 24-byte random nonce and the
XChaCha20-Poly1305 ciphertext and tag of a JSON message. The key is 32 bytes
of HKDF-SHA256 from that epoch's room media key with no salt and info
`rusttalk-control-v1`. The associated data is that literal, the epoch as a
big-endian i64, then the sender's and the recipient's LiveKit identity, each
preceded by its byte length as a big-endian u32. Receivers accept only the
current epoch and payloads up to 1024 bytes, open with the sender identity
LiveKit reports and their own, and drop anything that fails to authenticate,
decode or validate. Mouse moves go lossy; everything else is reliable.
The SFU alone can't read or forge these messages. As with media frames,
another member of the channel who holds the media key and controls the SFU
could claim a different sender identity.

| Message (`t`) | From | Fields |
| --- | --- | --- |
| `request` | Viewer | `request` (16 random bytes), `screen` (the screen's track ID, at most 64 bytes) |
| `withdraw` | Viewer | `request`: stop asking, or stop controlling |
| `grant` | Sharer | `request`, `session` (16 fresh random bytes) |
| `deny` | Sharer | `request`, `reason`: `denied`, `busy` or `unsupported` |
| `revoke` | Sharer | `request`: control taken back |
| `input` | Viewer | `session`, `seq` (u64), `input` |

`input` is one of `move {x, y}`, `button {button, down, x, y}` with button
`left`, `right` or `middle`, `scroll {x, y}` in wheel notches (positive
`y` scrolls up, positive `x` left, each at most 10 either way), and
`key {key, down}`. Positions run from 0 to 1 across the published picture.
`key` names a physical key by its US-layout position; only the keys listed in
`rusttalk_core::control::KEYS` (letters, digits, punctuation, F1–F12,
modifiers, navigation and editing keys) are valid, so the sharer's own
layout picks the character. The Windows key, media keys, Print Screen and
Pause are never sent.

Before anyone asks, the sharer says whether its screen takes requests in the
participant attribute `rusttalk.control-offer`, updated when that changes
(for example after switching the shared source). Its plaintext is `{track:
screen_track_sid, offer}`, `offer` being `on`, `off` (the sharer turned
requests off) or `unsupported` (this system can't inject input). It is sealed
like the phone consent attribute: the channel's video-watch-derived key, AAD
`rusttalk-control-offer-v1`, epoch (big-endian i64), sender identity, a zero
separator and sender session ID, wire form `epoch.base64url(ciphertext)`, at
most 1 KiB. Viewers disable "Request control" for `off` and `unsupported`. A
missing value, one for another track, an unknown `offer` or one that doesn't
open means unknown, and viewers may ask as before. It only saves a refused
request: the sharer still decides every request itself.

The sharer denies a request for a screen it isn't sharing, from a device it
can't match to a member, or while another request is open or someone is in
control (`busy`). A system that can't inject input answers `unsupported`
without asking. An unanswered request lapses after 30 seconds. An answer
names the request it answers, so a late "Allow" never grants a newer
request. The sharer injects `input` only from the granted device with the
granted `session`, and only when `seq` is higher than the last one seen on
the same lane (moves, or everything else), which drops replays and stale
moves. Control ends when the sharer stops it, the viewer withdraws or stops
watching, either leaves, or the share stops or switches to another source.
The sharer then releases held remote keys and buttons.

The viewer sends input only while the screen's tile (or its full screen
view) has the keyboard focus; holding Escape for one second stops
controlling. On Windows the sharer injects with `SendInput`: positions map
to the shared monitor, or to the shared window's visible frame, undoing any
letterboxing after a change of source. Pointer events that would land on
another window, keys while neither the shared window (or a window on the
shared monitor) has the focus, and anything aimed at RustTalk's own windows
are dropped; releases always go through. Windows never lets an app inject
Ctrl+Alt+Del, and it keeps input from reaching apps running as
administrator unless RustTalk does too.

On macOS, the sharer can enable control requests for each share, of a
display or a window, and RustTalk posts `CGEvent`s to the HID event tap.
That needs the Accessibility permission: until it is granted, the share
dialog says so and asks macOS to show its prompt, and requests are answered
`unsupported`. Every check fails closed: a window list, display list,
session state, focus or keyboard layout that can't be read blocks the event.

Positions map to the shared display's bounds, or to the shared window's
current frame. Clicks, drags and scrolls land only on an ordinary app
window: the topmost visible window under the pointer must be on layer 0,
belong to an app with a Dock icon (activation policy "regular") that isn't
RustTalk or a system process (WindowServer, Dock, SystemUIServer, Control
Center, Notification Center, loginwindow, SecurityAgent, coreauthd,
Spotlight), and be the shared window, or for a display share, be on that
display. The bare desktop, the top 40 points of every display (the menu
bar), a 4-point band along every display edge (where a hidden Dock or menu
bar slides in) and 24-point squares in the corners (hot corners) never get
remote pointer input. Nothing is posted while the login session isn't on
the console or the screen is locked.

Keys go through only while the Accessibility API reports the focused app
as an ordinary app that isn't RustTalk or a system process, its focused
window has the `AXWindow` role, the focused element is that window or
inside it, no popup or context menu is open and no menu element has the
focus, and that window is the shared window or has its centre on the
shared display. The shared window is recognised by its window ID, which
RustTalk reads with `_AXUIElementGetWindow` (private but long stable); if
that fails, the focused window's frame must match exactly one window of the
app, the shared one. Only "no value" or "attribute unsupported" answers
count as no focused element; any other Accessibility error, a timeout or an
unreadable role leaves the focus unknown and blocks the key.

Ctrl is posted as Command, so Ctrl+C, V and Z do what a viewer on Windows
or Linux expects; a viewer on a Mac sends Command as Ctrl. Alt is Option,
the Mac's own Control and Fn keys are never posted, and Menu is dropped.
F1 to F12 are never posted. With Command held, Tab, Space, Q and H are
blocked; with Command and Option, Escape, D, W and M; with Command and
Shift, 3 to 6 (the app switcher, Spotlight, quitting the whole app or
logging out, hiding apps, Force Quit, the Dock, closing or minimizing every
window and screenshots). On a window share, Command with ` or the ISO key
beside it (with or without Shift) is blocked too, since it moves the focus
to the app's other windows. A chord is checked by the key's US position and by the
character the key types with Command in the sharer's current keyboard
layout, read for each Command chord (so the AZERTY key that types Q is
blocked too); if the layout can't be read, the chord is blocked. Every
event carries only the modifiers the controller holds. A Command press is
held back until another key, a click or a scroll needs it, and a Command
press never posted isn't released, so a bare tap of Command (twice opens
Siri) never reaches macOS.

While a remote key or button is held, RustTalk rechecks the permission,
the session, the keyboard focus (for held keys) and the window under the
pointer (for held buttons) about every 100 ms. A failed check, or the
permission or session failing at any event, releases held remote input,
drops further input, and ends control at the next check of the share
(within about 2 seconds), which tells the viewer. Held keys and buttons are
also released when control ends or the share stops; releases macOS doesn't
take are forgotten, so stale modifiers never return. Marking a failure,
the cleanup and letting an event through share one lock, and every event
checks for a failure after taking it, so nothing is pressed after the
cleanup.

Limits: each check is a snapshot, so the window under the pointer or the
focus can change between the check and macOS delivering the event. Fully
transparent windows are ignored when finding the window under the pointer.
An app that reports no focused element counts as focused on its window,
and one that doesn't report its focused window gets no keys. A drag image
over the pointer while a button is held ends control.

On Hyprland, the sharer can enable control requests for each monitor share.
ScreenCast supplies the selected output's name, and RustTalk binds a Wayland
virtual pointer to that output. It uses a Wayland virtual keyboard with the
seat keymap and the active layout at the start of control. RustTalk's Allow
prompt is the only permission gate for input. Window shares, shares without
the opt-in or a usable output identifier, and other Linux compositors answer
`unsupported`. A source switch ends control and starts with a fresh opt-in.

Before a click, scroll or key event, RustTalk checks the pointer is on the
selected output through Hyprland IPC. Keyboard input also requires a mapped,
visible regular window on that output, as reported by `activewindow`.
Because Hyprland can keep reporting that window while a layer surface owns
keyboard focus, RustTalk checks mapped `layers` on all outputs. It drops keys
when a layer with exclusive or on-demand keyboard interactivity is on
another output or on any top or overlay layer. It uses the layer's IPC
interactivity field where available, or corroborates the layer address using
Hyprland's Lua layer query. Unknown interactivity in those positions blocks
keys.

RustTalk reads the configured `binds` and active `submap` at control start and
again when verifying clicks, scrolls, keys or held input. It checks XKB
modifiers, keysyms and keycodes against the active and universal binds.
Hyprland resolves keysym binds with a keymap built from the global
`input:kb_rules`, `kb_model`, `kb_layout`, `kb_variant` and `kb_options`
options, which can differ from the seat keymap the virtual keyboard uses.
RustTalk reads those options with `getoption`, builds the same keymap and
blocks a key whose keysym under either keymap, or whose keycode, matches a
bind. Like Hyprland, a key that resolves to a modifier keysym such as
`Control_L` counts its own modifier bit when matching. Keys are blocked if the options can't be read or compiled, or if
`input:kb_file` is set. Buttons match `mouse:272`-style triggers and scrolls
match `mouse_up`, `mouse_down`, `mouse_left` and `mouse_right` in either
direction, with the virtual keyboard's modifiers, including release, click
and drag binds and `bindm`. Catch-all binds in the active submap block
pointer input too. A new modifier is checked against held buttons, and held
buttons are rechecked with held keys. RustTalk cross-checks the text bind
list for triggers omitted from JSON. Hyprland's JSON bind list omits the
flag that says a bind ignores modifiers, so RustTalk assumes Hyprland's
default: a bind fires only with exactly its modifiers. A bind explicitly
flagged to ignore modifiers is not detected and can still fire with other
modifiers held. Super, Logo and Mod4,
including remapped keys, and Ctrl+Alt with Delete or a function key are
blocked. VT switch keysyms (`XF86Switch_VT_1` to `XF86Switch_VT_12`),
`Terminate_Server` and the other server-private `XF86` keysyms that the
compositor can act on before binds are blocked with any modifiers. RustTalk
drops stale or failed events. Local pointer or keyboard activity pauses
input until the seat is idle for about 500 ms.

About every 100 ms while a remote key or button remains held, RustTalk
rechecks the lock state, monitor, pointer, keyboard focus and binds even
without incoming events. A failed check releases held remote input and
destroys the virtual devices. Cleanup clears the virtual modifier mask
before sending key releases.

There is a time-of-check/time-of-use race between the IPC snapshot and the
compositor delivering an event. The pointer, focus, lock state or bind
configuration can change in between. Hyprland reports local and virtual input
through the same idle protocol, so coincident local activity can be
coalesced into a virtual event's notification. The pause and periodic checks
reduce exposure, and release events still follow compositor routing.
Compositor actions outside the reported binds may affect other outputs.
Hyprland combines modifiers from all keyboards when matching binds, but its
IPC reports only Caps Lock and Num Lock, not the modifiers held right now.
RustTalk matches binds with the virtual keyboard's modifiers only. If the
sharer holds a physical modifier such as Super for more than about 500 ms
without other local input, a viewer's click, scroll or key can trigger the
sharer's bind for that modifier, for example `SUPER + mouse:272` moving a
window. This needs the sharer to hold the modifier themselves.

The API gets the active roster from LiveKit's internal RoomService,
cached for one second per room, and maps device identities to current
channel readers. Multiple devices for one person produce one entry in
`people`. Active devices that match no current reader count toward
`unknown`; clients show them as unknown devices. An empty room returns
`{"people": [], "unknown": 0}`.

After a kick, ban, device revocation, role assignment, ownership
transfer or channel permission change, and on each LiveKit join notice,
the API removes devices without read access from every voice room with
RoomService `RemoveParticipant`. It uses `UpdateParticipant` to revoke
lost audio, video and data grants. An existing room session
keeps its other publication rights. Rejoin to pick up a new grant. These calls run after the change
commits. A failure is logged with a reason and never undoes the change.
Errors include `403 channel_read_denied`, `400 not_a_voice_channel`,
`502 livekit_roster_unavailable` and
`503 voice_unavailable`.

### Terminal sharing

A member of a call can share a terminal: a shell runs in a pseudo terminal
on their device only (ConPTY on Windows), and viewers replay its output in
their own terminal emulator at the sharer's size. Viewers never resize it.

The sharer announces the share as the participant attribute
`rusttalk.terminal`: `<epoch>.<base64url>`, the decimal channel-key epoch,
a dot, then unpadded base64url of a 24-byte nonce and the
XChaCha20-Poly1305 ciphertext and tag of the JSON `{"id": [16 bytes]}`. The
key is HKDF-SHA256 from that epoch's room media key with info
`rusttalk-terminal-share-v1`; the associated data is that literal, the
epoch as a big-endian i64 and the sharer's LiveKit identity. An empty value
ends the announcement. Receivers open it with any epoch key they hold.

Messages are LiveKit data packets with topic `rusttalk.terminal`, always
reliable and sent to one device, sealed exactly like remote control
messages with HKDF info `rusttalk-terminal-v1` in place of
`rusttalk-control-v1`, up to 15 KiB on the wire. Byte fields are base64
strings. Every message carries the share's `id`; others are dropped.
The SFU alone can't read or forge them. As with remote control and media,
another member of the channel who holds the media key and controls the SFU
could claim a different sender identity, and so read a terminal or send
messages in another device's name.

| Message (`t`) | From | Fields |
| --- | --- | --- |
| `watch` | Viewer | Send me the screen and the output from then on; also sent after a gap |
| `unwatch` | Viewer | Stop sending |
| `snapshot` | Sharer | `seq`, `cols`, `rows`, `data` (at most 8 KiB), `more` |
| `data` | Sharer | `seq`, `data`: output bytes, at most 8 KiB |
| `resize` | Sharer | `seq`, `cols`, `rows` |
| `end` | Sharer | The share stopped |
| `request` | Viewer | May I type? |
| `withdraw` | Viewer | Stop asking, or stop typing |
| `grant` | Sharer | `session` (16 fresh random bytes) |
| `deny` | Sharer | No, or the request lapsed |
| `revoke` | Sharer | Typing taken back |
| `input` | Viewer | `session`, `seq` (u64), `first` (starts a key press or paste), `data`: typed bytes, 1 to 1024 |

Sizes run from 2 to 500 columns and 1 to 200 rows. Output and resizes are
numbered by one rising `seq`. A snapshot is the screen as of `seq`, as bytes
that redraw it, with up to 500 lines of history, in a fresh terminal of
that size; it is split over messages with the same `seq`, the last with
`more` false, and a viewer assembles at most 2 MiB. While a full-screen
program runs only its screen can be read, so the viewer gets the rest once
it ends. A viewer plays output whose `seq` follows the last one it has,
ignores older ones, and on a gap asks again with `watch`. Anyone in the
call may watch; only watchers get output. The sharer gathers output every
33 ms. More than 32 KiB in that time is dropped and every watcher gets a
fresh screen instead; each watcher gets at most one screen every 500 ms.

A watcher can ask to type. The sharer must allow each person; nothing is
allowed automatically, Allow can't be clicked until the prompt has been up
for a second with the window in front, and a request nobody answers lapses
after 30 seconds. After a no or a lapse, that device's requests are ignored
for 30 seconds, and after three for the rest of the share. Several people
may type at once, and the sharer sees who and can take it back from each.
The sharer denies a request from a device that doesn't watch, or while its
own server connection is down, since it can't then tell who asks. The
viewer encodes keys as a terminal does (xterm sequences, control
characters, bracketed paste with Escape and C1 controls removed) and sends
them as `input`; it refuses a paste over 16 KiB. The sharer writes `input`
to the shell only from a device it allowed, with that grant's `session`, a
`seq` higher than the last one seen, while its server connection is up,
and within a budget of 16 KiB at once refilled at 8 KiB per second;
anything else is dropped. Once part of a key press or paste is dropped, the
rest of it is too, until the next `input` with `first`. Typing ends when the
sharer takes it back, the viewer withdraws or stops watching, either
leaves, or the share stops; what was already started keeps running. Typing
is a remote shell: a member of the channel who also controls the SFU could
read the grant and type as the allowed device.

Stopping the share ends the shell and sends `end` to the watchers. Leaving
the call does too. The terminal's own answers to programs (cursor position,
device attributes) come from the sharer's emulator only, and neither side
lets output read or write the clipboard. A viewer takes at most 4 MiB at once
from a share, refilled at 2 MiB per second, and asks for a fresh screen past
that; it keeps at most 8 combining marks in a row. A snapshot that would be
bigger than 2 MiB loses history first, then its colours.

## Live events

Open one `GET /v1/events` WebSocket per connection with the bearer
Authorization header. Use a query-less URL. The server sends JSON text
frames, each up to 4,096 bytes so far; apps accept up to 16 KiB. It sends the
outstanding key work at connection time and checks session validity before
pushing updates.

| Type | Other fields |
| --- | --- |
| `keys_needed` | `scope`, `epoch`, `device`. Reconcile pending wraps and delivered keys. Completion also wakes the recipient |
| `rotate_needed` | `scope`. Fetch the current epoch and try to publish its successor |
| `member_changed` | `person`, optional `device`, optional `profile`. Refresh the member and device directory and catalog. With `profile` (`display_name_ct`, `avatar_hash`, `bio_ct`, `hide_offline`, `avatar_initialized`, `voice_muted`, as `GET /v1/members` lists them) only that member's profile changed: update that member, no key or directory reload needed. Left out when the notice would exceed 3,800 bytes |
| `presence_changed` | `person_id`, `status`. Sent to members when visible presence changes |
| `channel_changed` | Optional `part`. Refresh the catalog, server metadata, server emoji and server events. With `part`, only that changed, and no keys or access: `channels` (the catalog: channel and category names, topics, order, settings, focus mode), `space` (`GET /v1/space`), `emoji` or `calendar`. Treat an unknown `part` like none |
| `voice_changed` | `channel`, optional `people` and `unknown` as from GET `/v1/voice/{channel}/participants`. Without them (older servers, or a roster too big for one frame), fetch current voice participants. Sent to current channel readers on signed LiveKit join, leave and room-end notices |
| `voice_moved` | `devices`, `from`, `to`. A moderator moved these devices' call; join `to`. Sent only to those devices |
| `dm_created` | `channel`. Refresh the catalog and reconcile keys |
| `message_created` | `channel`, `id`, optional `record` (the ciphertext record, left out when the event would exceed 3,800 bytes). Verify and open `record` like a fetched one; without it, fetch the record |
| `message_deleted` | `channel`, `id`. Remove the record and hide edits that reference it |
| `live_file_accepted` | `channel`, accepting `device`, signed `acceptance` object, optional `data` (true for data-room acceptances). Sent only to the offering device while it has channel read access |
| `resync` | None. The socket fell behind. Fetch current state, including keys, catalog, member presence and devices, and visible message history |

The API closes the socket when the token expires or ends, including kick,
ban and device revocation. Close reasons are fixed strings and logged.
The server keeps lagging sockets open and recomputes the outstanding
key work. DM, message and voice notices are sent only to current channel readers.
Fetch the roster when loading a channel list and after `resync`.
A member’s first event socket announces their stored choice, which starts as
`online`; an `invisible` member is not announced. The choice applies across
sockets and devices and survives reconnects and server restarts. The directory
and notices use `offline` for invisibility and for disconnected members. The
last socket closing starts a one-second reconnect grace period before visible
presence goes offline. Notices for one member are coalesced to at most one per
second; the latest choice wins, and a choice reverted within that second sends
nothing.

## Files

Files use the same bearer and channel permissions as messages. Each file ID
is a random 32-byte value chosen by the sender before encryption and encoded
as unpadded base64url. The sender encrypts the file before making the request.
An image thumbnail is a separate blob with its own ID and key, authorization,
quota charge and expiry. DMs grant read and write to both participants.

| Method and path | Body | Permission and success |
| --- | --- | --- |
| POST `/v1/files?channel={channel}&id={id}` | Stream of raw ciphertext with `application/octet-stream` | Channel `write`. 201, `{id}` |
| GET `/v1/files/{id}` | Empty | Channel `read`. 200, streamed ciphertext with `Content-Type: application/octet-stream` and `Content-Length` |
| DELETE `/v1/files/{id}?uploader={person}` | Empty | Uploader, or a reader of a non-DM channel with `delete_messages` when `uploader` names the file's uploader. 204; in a DM only the uploader (`403 permission_denied`) |
| POST `/v1/channels/{channel}/files` | `{id, epoch, envelope}` | Channel `write`, for the caller's own unexpired file stored in that channel. 204 |
| GET `/v1/channels/{channel}/files` | Empty | Channel `read`. 200, all of its (at most 500) shared files newest first, each a message record |

The upload rejects a blob below 16 bytes and caps the bytes as they arrive,
including when the request has no Content-Length. An oversized upload is read
to its end and discarded before the 413, so proxies do not report a reset
stream. A file ID is never reused, even after its file expired or was deleted. The configured limits count
ciphertext bytes, including tags and thumbnail blobs. Upload errors include
`413 file_too_large`, `413 file_member_quota`, `413 file_server_quota`,
`409 file_exists`, `408 file_upload_timeout` and `503 file_upload_busy`. The
API stages at most four uploads at once and at most two per member. An upload
fails with 408 when no bytes arrive for 30 seconds or it takes over 15 minutes.
Lower limits apply to new uploads. Shorter retention also applies to existing
blobs during the next cleanup.
A moderator deleting a message passes its sender as `uploader`; the
optional parameter is ignored for the uploader's own files. When it is
missing or names someone else, a moderator gets `404 file_not_found`, so a
message that references another member's file cannot get it removed.
A missing or expired blob returns `404 file_not_found`. A download without a
valid session also returns 404. A member without channel access gets
`403 channel_read_denied`. Only the upload route uses the file body limit.
The default JSON cap is 4 KiB, with the existing scoped chat and encrypted
profile/space metadata limits.

The encrypted and signed [message plaintext](crypto.md#messages) has a
one-byte attachment count from 0 to 4, followed by that many references
of at most 529 bytes each.
Each reference is encoded in this order.

| Field | Encoding |
| --- | --- |
| File | A blob reference |
| Name | One-byte length, then a [file name](crypto.md#messages) |
| Media type | One-byte length, then 1–127 printable ASCII bytes excluding spaces |
| Thumbnail present | Byte 0 or 1 |
| Thumbnail | Blob reference when present. Plaintext at most 512 KiB and ID distinct from the file ID |

A blob reference contains a 32-byte raw file ID, a 32-byte key, a four-byte
plaintext size and a four-byte chunk size. Integers are unsigned big-endian.
Readers accept power-of-two chunk sizes from 16 KiB through 1 MiB, with
64 KiB used by senders. The client bounds the full ciphertext to 32 MiB.
Recipients treat the name as display text and choose a local save path
independently. The filename and media type are never part of the HTTP request.

#### Shared files

A channel's file list holds stored files shared there directly, outside any
message. The client uploads the file without a thumbnail, then shares it with
an `envelope` built like a signed [message](crypto.md#messages) in the
channel's current epoch, under its own shared-file label and with the file ID
as the message ID. Its plaintext has no
flags, empty text and exactly that one file reference without a thumbnail;
readers skip any record that holds anything else. The API checks the
signature (with the shared-file label) and epoch like a message post (`409 epoch_changed` on a stale
epoch), answers `409 file_shared` when the file is already shared and
`404 file_not_found` for a file that is missing, expired, someone else's or
stored in another channel. The envelope is at most 1,024 bytes
(`400 invalid_message` past that), and a channel keeps at most 500 shared
files (`409 shared_limit`). A shared file no longer expires; it counts toward
the same quotas and is removed with the existing DELETE route (its uploader,
or a moderator naming the uploader) or when its channel is deleted. When
its sharer leaves, is kicked or banned, or revokes the device that shared it,
the file leaves the list and expires like any file again. The list
returns the same record fields as a message page, and
`/v1/channels/{channel}/message-devices` also finds senders of shared files.
Apps refresh the list when it is opened; no event announces changes.

#### Link previews

A sender may attach a preview of one link in the message text as an ordinary
stored file, so the format and the API are unchanged. Its media type is
`application/vnd.rusttalk.link-preview+json`, its name `link-preview.json`,
and its plaintext at most 4,096 bytes of UTF-8 JSON:

```json
{"url": "https://example.org/story", "title": "…", "description": "…", "site": "…"}
```

`url` (an `http` or `https` address with a host and no user name or password,
at most 2,048 bytes) and a non-empty `title` are required; `description` and
`site` may be missing. The optional thumbnail is the page's picture, made like
any other thumbnail. Apps that know the type download the file, show it as a
card under the text instead of as a file (at most one per message) only when
`url`, parsed, equals one of the message text's parsed link destinations,
show the address's host on it, treat every field as plain text and cut the title to 200,
the description to 300 and the site to 80 characters. They ignore a file of
this type that is bigger, unreadable or invalid. Older apps show it as a small
file. Senders only make previews for links that no reader-side link preview
(see [Privacy](privacy.md)) covers.

Message deletion and history pruning do not reveal the encrypted reference
list to the API. When deleting a message, the client calls DELETE for its
attachment IDs, including thumbnail IDs, while it still has the references.
Unreferenced blobs expire on schedule. Deleting a channel marks its blobs for
the next sweep.

### Live transfer

A live offer is an original signed, encrypted chat message in any readable
and writable channel or DM with flag bit 2 set and the offer as the last
[plaintext field](crypto.md#messages). The offer in order is a random
32-byte transfer ID, a random 32-byte file key, the plaintext length
(u64, at most 2 GiB), the chunk size (u32, 65,536), the file name byte
length (u8) followed by the UTF-8 [file name](crypto.md#messages), and
the MIME type byte length (u8) followed by the ASCII type (1–127
printable bytes). It is at most 460 bytes. Integers are big endian. The
sender guesses the MIME type from the extension and uses
`application/octet-stream` when unknown. The name is only displayed. The
receiver never derives a save path from it.

The receiver chooses a destination and explicitly accepts an offer from a
currently verified device. For a transfer outside a call, each participant
uses `POST /v1/channels/{channel}/live-file/token/{nonce}` with a bearer token
and an empty body. The route requires a text channel, voice channel or DM the
caller can read, otherwise `403 channel_read_denied`, and returns
`{token}`. DM participants can read and write their DM. The token allows
joining for 60 seconds and is scoped to the `file-{channel}` LiveKit room.
It allows subscribing, and publishing data only with write access, so a
reader without write can receive but not send. Media publication is disabled,
including for members with voice privileges. The client disables automatic
track subscription and connects the receiver when they choose Save. The
sender connects on a verified acceptance. The participant identity is the
usual opaque LiveKit device identity followed by a dot and the acceptance
nonce. Each acceptance has its own connection, which closes after success,
failure or cancellation.

After joining, the receiver sends `POST /v1/channels/{channel}/live-file`
with a bearer header and JSON `{recipient, transfer, nonce, cancel, signature}`.
To accept, the accepting device must read the channel and be in the data
room, and the offering device must be allowed to write. Membership checks
remove a data-room participant who loses read access, and a sender who loses
write access. The API returns 204 after queuing a volatile
`live_file_accepted` event to the offering device. In-call transfers also
support the existing `POST /v1/voice/{channel}/live-file` route with the same
body. It requires voice channel read access and both devices active in the
voice room. A cancellation can be relayed without an active room connection.

`recipient` is the offerer's Ed25519 public key. The transfer ID and
receiver-generated random 16-byte nonce are encoded in base64url without
padding, as are the recipient and signature. `cancel` is false to accept
and true to cancel that acceptance. Both the API and offerer verify the
acceptance signature. Each offer can serve each accepting device once and
accepts new recipients for ten minutes or until the sender cancels it or
ends the in-call transfer session.

The sender opens an incremental LiveKit byte stream on topic
`rusttalk.file`, with a singleton `destination_identities` list for
that accepting device. The header has `transfer` and `acceptance`
attributes containing the encoded transfer ID and acceptance nonce. It
declares the ciphertext size and carries neither the file's name nor
its MIME type. The receiver matches the topic, both attributes, declared
size and LiveKit sender identity against its pending acceptance before
writing anything.

The stream body is the concatenation of sealed chunks in zero-based
index order, without compression. Each chunk is the ciphertext of the
next 65,536 plaintext bytes (or the remaining bytes for the last chunk)
followed by a 16-byte tag. An empty file has one authenticated empty
final chunk. Chunk boundaries, indices and the final flag are derived
from the authenticated offer size. With the 2 GiB plaintext limit the
maximum ciphertext size is 2 GiB plus 512 KiB. Clients reject missing
chunks and extra bytes and only publish a temporary download to its
chosen path after verifying the final chunk and stream end. The chosen
path must be new.

## Sync

Both routes require a member bearer token. The API selects the row using the
session's person ID.

| Method | Request body | Response |
| --- | --- | --- |
| GET `/v1/me/sync` | Empty | 200 with the latest blob, or 204 if absent |
| PUT `/v1/me/sync` | Sync blob | 204 if stored, or 409 with the current version if the submitted version is equal or lower |

A blob is an eight-byte, positive signed integer version in big-endian order
followed by the encrypted payload (24-byte nonce, ciphertext, 16-byte tag).
The payload limit is 256 KiB including the nonce and tag. The PUT body limit
is 256 KiB plus eight bytes. Malformed framing returns 400 and an oversized
body returns 413. Successful GET and conflict responses use
`application/octet-stream`. The conflict body is exactly the eight-byte
version field. A device reloads and merges before retrying a conflict.

Core takes the highest authenticated version from the member servers it can
reach, breaking ties between replicas by lexicographically greatest origin.
Merge works per origin and per field: the newer snapshot wins for anything
both sides mention, local wins ties, and entries present on only one side are
retained. A contact that both sides hold with a `changed` time keeps the later
change instead. Leaving a server records its origin in `left`, and removing a
setting or contact sets it to `null`, so an older or replayed blob cannot bring
it back. Pin conflicts are reported and never adopted; local and saved pins
stay. Devices persist their snapshot and version so a server cannot roll them
back by serving an older blob.
Apply pending local edits after merging and publish the next version to all
current peers. A failed peer is reported separately and can catch up after
reconnect. A restored device needs a verified member server as a starting point.
The desktop client syncs on each connect and after each local change.
A device restored from the words signs in to one server it is a member of
(members need no invite) and then picks up the rest of the list from that
server's blob. Before asking for a display name, the desktop client checks a
server picked on the join page by signing in without an invite: if that works
the person is a member already, keeps their name and picture, and is signed
in without a name step. The sync runs as soon as a server is signed in, ahead
of loading its channels.
Leaving a server in the client calls `DELETE /v1/me/membership` and then
removes the server from the person's devices.

## Public index

### Owner setting on the chat API

`GET /v1/settings/public-listing` and `PUT /v1/settings/public-listing`
require an owner bearer token. GET returns the current setting, initially
`{"enabled":false,"listing":null}`. PUT replaces the setting and returns
204 after saving it. Registration runs in the background.

```json
{
  "enabled": true,
  "listing": {
    "name": "Clover",
    "description": "A community for friends",
    "address": "https://chat.example.org",
    "index_url": "https://directory.example.org",
    "index_tls_fingerprint": null
  }
}
```

Supply the advertised HTTPS origin as `address`. The API appends
`/v1/servers` to `index_url`, which may include a path prefix such as
`/index`. Neither URL accepts credentials, query strings or fragments.
The optional `index_tls_fingerprint` is the 32-byte SHA-256 fingerprint
of the index's DER leaf certificate encoded as unpadded base64url. Omitting
it uses the API container's CA bundle. The publisher follows no redirects.

The API registers again every hour and retries after five minutes when a
registration fails. It logs failures with a short reason and, when the index
rejects the registration, the index's error code.

Send `{"enabled":false}` to clear the configured publication and stop renewals.
The existing listing remains visible until its lease expires. Changing the
directory URL also lets the old lease expire. Public text is supplied
explicitly and the API counts rows in its members table. The listing
reports the server's current open joining setting.

### Index HTTP API

The index uses its own HTTPS base URL and no bearer token.

| Method and path | Request | Success |
| --- | --- | --- |
| GET `/v1/health` | Empty | 200, `ok` |
| GET `/v1/servers?q=&page=1` | Optional literal search string and one-based page | 200, array of up to 25 listing objects |
| POST `/v1/servers` | `{listing, timestamp, signature}` | 204, accepts or refreshes a listing |

A listing is `{server_id, name, description, address, member_count,
open_joining}`. The name must contain a non-whitespace character and
have at most 100 Unicode scalar values. The description may be empty
and has at most 1,000 scalar values. The name permits no control
characters and the description permits tab, carriage return and newline.
Neither permits bidi embeddings, overrides or isolates (U+202A to U+202E,
U+2066 to U+2069) or the invisible U+200B, U+2060 and U+FEFF.
The address is an HTTPS origin of up to 512 bytes with an optional trailing
slash. IPv6 hosts use brackets. `member_count` is an unsigned 32-bit integer
and `open_joining` is a JSON boolean. The POST and owner PUT limits are
16 KiB of JSON.

Search matches a literal substring of name or description using SQLite LIKE
with ASCII case folding. An omitted or empty `q` browses all listings.
`q` has a maximum of 100 Unicode scalar values. Pages 1 through 1,000 are
accepted. The index orders rows by name with ASCII case folding, then by
server ID. Use a short or empty page to detect the end of results. Listings
can change between pages. A directory result supplies an advertised server
ID. Connect using the normal TLS trust or pin and verify a freshly signed
server challenge against that ID. A listing does not carry a TLS pin.

Registration timestamps are **Unix milliseconds**. The Ed25519 signature is
64 bytes, encoded as unpadded base64url. The bytes to sign are:

```text
"rusttalk-index-v1" || server_id (32 raw bytes) ||
timestamp (i64, big endian) || member_count (u32, big endian) ||
open_joining (one byte, 0 or 1) ||
len(name) (u16, big endian) || name (UTF-8) ||
len(description) (u16, big endian) || description (UTF-8) ||
len(address) (u16, big endian) || address (UTF-8)
```

Lengths count UTF-8 bytes. There is no domain terminator or normalization.
The index rejects timestamps more than 60 seconds from its clock in either
direction, and requires each accepted timestamp to exceed the last one for
that server ID. Expiry is three hours from acceptance. Registration allows
16 attempts per minute per IPv4 address or IPv6 /64. Registration errors use
`400 invalid_listing`, `400 stale_registration`, `401 invalid_signature`,
`409 replayed_registration`, `429 registration_limit` or
`503 index_full` when a new server ID would exceed the index's listing cap
(refreshes of an existing listing are still accepted). Invalid search
parameters return `400 invalid_query`.
