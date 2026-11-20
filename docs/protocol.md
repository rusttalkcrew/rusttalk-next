# Protocol

The API runs over HTTPS. Requests with JSON bodies use `Content-Type: application/json`.
The default request body limit is 4096 bytes. Binary fields use canonical unpadded
base64url. Times are Unix seconds. Auth and membership responses carry
`Cache-Control: no-store`.

## Sign-in

| Method and path | Request | Success |
| --- | --- | --- |
| GET `/v1/health` | Empty | 200, `ok` |
| GET `/v1/auth/challenge` | Empty | 200, `{challenge, server_id, signature}` |
| POST `/v1/auth/sign-in` | `{person_id, certificate, challenge, signature, invite?}` | 200, `{token, expires_at}` |
| POST `/v1/auth/refresh` | Bearer token, empty body | 200, `{token, expires_at}` |

The server signs the 32 raw bytes of the challenge followed by the ASCII
bytes `rusttalk-server-v1`. The client verifies the Ed25519 signature against
the expected 32-byte server ID, then signs these bytes with its device signing key:

```text
challenge (32 bytes) || server_id (32 bytes) || "rusttalk-auth-v1" (16 ASCII bytes)
```

Domain strings have no trailing terminator. Signatures are 64 bytes. The
certificate uses the public W06 format described in [Crypto](crypto.md). Use a fresh
challenge per attempt. The server consumes a recognized challenge before
verifying the device proof, and rejects challenges after 60 seconds. At most
256 challenges are outstanding across the server.

Members sign in with their device proof. For a new member, supply the invite
code as the optional `invite` field. Invite redemption, membership and session
creation are atomic. Each person and device pair can have one current session.
Successful sign-in replaces any previous token for the device. A refresh
rotates the bearer token and extends expiry to 12 hours from the refresh.
A person may register up to ten devices. At the cap, sign-in from a new
device returns `409 device_limit`. Revoking a device frees a slot.

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
| GET `/v1/me` | Empty | Any member. 200, `{person_id, device_id, joined_at, role_ids}` |
| POST `/v1/me/revocations` | `{record}`, a W04 root-signed revocation for the caller's person | Any member. 204, affected device loses its token |
| POST `/v1/invites` | `{uses, ttl_seconds, space_key_ct, name_ct?}` | `invite`. 201, `{id, code, expires_at}` |
| DELETE `/v1/invites/{id}` | Empty | `invite`. 204 |
| DELETE `/v1/members/{person}` | Empty | `kick`. 204 |
| PUT `/v1/bans/{person}` | `{ban_ip?}`, default false | `ban`. 204 |
| DELETE `/v1/bans/{person}` | Empty | `ban`. 204 |
| GET `/v1/roles` | Empty | Any member. 200, `{default_role_id, roles}` |
| POST `/v1/roles` | `{id?, rank, permissions, name_ct?}` | `manage_roles`. 201, the created role |
| PUT `/v1/roles/{id}/name` | `{name_ct?}` | `manage_roles`. 204 |
| DELETE `/v1/roles/{id}` | Empty | `manage_roles`. 204 |
| PUT `/v1/roles/default` | `{role_id}` | `manage_roles`. 204 |
| PUT `/v1/members/{person}/roles` | `{role_ids}` | `manage_roles`. 204, replaces assigned roles |
| PUT `/v1/owner` | `{person_id}` | Owner. 204, transfers ownership to a member |
| GET `/v1/audit?after=0` | Empty | `view_audit_log`. 200, an array of up to 100 audit entries in ascending ID order |

A role has `{id, kind, rank, permissions, name_ct}`. A member's permissions
are the union of their role flags, and their highest rank is used for
hierarchy. Owner (1000) and Admin (100) have all seven flags. Moderator (50)
has `kick`, `ban`, and `delete_messages`. Member (0) has no flags and starts
as the default. The other flags are `invite`, `manage_roles`, and
`view_audit_log`, and `manage_channels`. Built-in role IDs
match their kinds. Custom roles have opaque IDs and ranks from 1 to 999,
strictly below their creator. They can only receive flags that the creator
has. `name_ct` is null or a metadata ciphertext of at most 512 bytes
after decoding. Supply a client-generated 16-byte ID when setting an
initial name. The server can assign the ID for an unnamed role. Use the
role ID's UTF-8 bytes as the metadata object ID, including for built-ins.
W05 role label blobs had no epoch or required encoding. The upgrade clears
those labels and preserves the roles and assignments. The server allows
up to 128 roles including built-ins.
Deleting a custom role requires clearing its assignments and replacing it
as the default first. Changing the default affects future joins.

Kick, ban, and role assignment require the actor to outrank the target
strictly. Granting a role or choosing it as the default also requires the
actor to outrank that role and hold every permission it grants. The
owner role comes from a singleton ownership record and can only move via
the transfer endpoint. The prior owner keeps their assigned roles.

Invite uses may be 1–1000, with lifetimes from 1 second to 30 days. The
bootstrap invite is single-use and expires after a day. For a regular
invite, supply the encrypted space key and, if the server has a name,
a separately encrypted copy of that name. Encrypt the copy using the
current space epoch even if the server's stored metadata uses an older
epoch. An invite issued before rotation can still preview the snapshot
from issuance. Kick and ban
remove membership and drop all its sessions. Unban clears the person ban
and any linked IPs. The former member needs an invite to join again. When
`ban_ip` is true, the server adds the target's IPs from active sessions.
It refuses an IP ban when no such IP is available or when an address is
shared with the actor or an equal or higher-ranked active member. The
owner retains access from a banned IP so an IP ban cannot lock them out.

Each audit entry has `{id, actor_person_id, action, target_person_id, target_id, at}`.
A target field is null if the action has no target of that type. Use the
last returned ID as the next `after` cursor. Audit entries contain action
names and IDs.

## Errors

Application errors use `{"error":"code"}`. General HTTP routing and transport
errors may have an empty body. The API logs a request ID, status and reason.

| Status | Codes |
| --- | --- |
| 400 | `invalid_json`, `invalid_encoding`, `invalid_query`, `invalid_forwarded_ip`, `invalid_revocation`, `invalid_invite_limits`, `invalid_role_rank`, `role_name_too_large`, `invalid_role_count`, `already_owner` |
| 401 | `invalid_challenge`, `invalid_certificate`, `invalid_device_signature`, `invalid_session`, `token_expired` |
| 403 | `invite_required`, `invalid_invite`, `person_banned`, `ip_banned`, `device_revoked`, `permission_denied`, `role_hierarchy`, `cannot_grant_permission`, `built_in_role`, `owner_required` |
| 404 | `invite_not_found`, `member_not_found`, `ban_not_found`, `role_not_found` |
| 409 | `already_banned`, `no_active_ip`, `role_in_use`, `role_limit` |
| 413 | `body_too_large` when generated after reading the body |
| 415 | `expected_json` |
| 429 | `challenge_limit` |
| 500 | `database_error`, `database_pool_timeout`, `database_busy`, `clock_error`, `randomness_error`, `invalid_stored_person` |

## Keys and encrypted metadata

All routes in the table require a bearer header except invite preview.
A scope is `{kind, id}` where kind is `space` or `channel` and the ID is
the server ID or the channel ID. Epochs are positive integers, with 0
in a state response indicating that the first epoch is still pending.

| Method and path | Body | Permission and success |
| --- | --- | --- |
| GET `/v1/member-devices` | Empty | Any member. 200, `[{person, certificate}]` of current devices |
| GET `/v1/keys` | Empty | Any member. 200, `{scopes, envelopes, pending}` |
| POST `/v1/keys` | `{scope, epoch, device, envelope}` | A reader of the scope. 204. The recipient must also be a current device of a reader |
| POST `/v1/keys/reject` | `{scope, epoch, sender, envelope}` | A reader of the scope. 204. Deletes a matching envelope addressed to the caller's device |
| POST `/v1/keys/epochs` | `{scope, epoch, commitment, envelope}` | A reader of a rotating scope, or the owner for the initial space epoch. 201. The envelope is addressed to the caller's device |
| GET `/v1/space` | Empty | Any member. 200, `{name_ct, description_ct, icon_ct}` |
| PUT `/v1/space` | `{name_ct?, description_ct?, icon_ct?}` | Owner. 204 |
| GET `/v1/members` | Empty | Any member. 200, `[{person_id, display_name_ct, avatar_ct}]` |
| PUT `/v1/me/profile` | `{display_name_ct?, avatar_ct?}` | Updates the authenticated member's own profile. 204 |
| POST `/v1/invite-preview` | `{code}` | No bearer. 200, `{space_key_ct, name_ct}`. Missing, expired, spent or unpreviewable invite returns 404 |

Scope state entries have `{scope, epoch, rotate}`. Pending entries have
`{scope, epoch, device}`. Envelope entries add `sender`, `commitment` and `envelope`
to those fields. Each response exposes envelopes only for its authenticated
device and readable scopes. Pending wraps include all epochs, and include
records whose sender device is no longer current. Each new epoch is
created in one transaction with its first envelope. A client that loses
the epoch race receives `409 epoch_changed` and fetches state again.
Rejecting a matching envelope reopens its slot and prevents the sender's
person from uploading to that slot while they remain a member. A stale
rejection leaves the current envelope in place.
See [Crypto](crypto.md) for the binary envelope and metadata formats.

Metadata PUTs replace the supplied record. Omitted or null optional fields
clear existing values. Metadata writes require the current space epoch.
During space rotation, the API returns `409 space_key_unavailable`.
A ciphertext from a different epoch returns `409 metadata_epoch_changed`.
Server names, descriptions, channel names and topics, category names and
display names allow 1024 plaintext bytes each. Icon and avatar ciphertext
blobs allow 256 KiB each. The two metadata PUT routes that accept images
allow 356 KiB JSON bodies to account for base64url and accompanying fields.
Invite preview permits eight lookups per IPv4 address or IPv6 /64 and
256 server-wide in 60 seconds. Invalid codes that pass base64url parsing also count.

## Channels and categories

All routes require a bearer header. Channel kind is `text`, `voice`, or `dm`.
Channel and category IDs are client-generated random 32-byte values.
`position` is an integer used for ordering with ID as the tiebreaker.
Text and voice channels each have one category. Channel kind is fixed at creation.

| Method and path | Body | Permission and success |
| --- | --- | --- |
| GET `/v1/catalog` | Empty | Any member. 200, `{channels, categories}` filtered to channels they can read and their categories |
| GET `/v1/channels` | Empty | `manage_channels`. 200, channel list |
| POST `/v1/channels` | `{id, kind, category_id, position, name_ct, topic_ct?, commitment, envelope}` | `manage_channels`. 201. Envelope initializes epoch 1 for the caller |
| PUT `/v1/channels/{id}` | `{category_id, position, name_ct, topic_ct?}` | `manage_channels`. 204 |
| DELETE `/v1/channels/{id}` | Empty | `manage_channels`. 204 |
| PUT `/v1/channels/{id}/permissions/{role}` | `{flags}` | `manage_channels` and a lower-ranked target role. The Owner can also change the Owner role. 204 |
| GET `/v1/categories` | Empty | `manage_channels`. 200, category list |
| POST `/v1/categories` | `{id, position, name_ct}` | `manage_channels`. 201 |
| PUT `/v1/categories/{id}` | `{position, name_ct}` | `manage_channels`. 204 |
| DELETE `/v1/categories/{id}` | Empty | `manage_channels`. 204 if empty, 409 if in use |

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

## Direct messages

| Method and path | Body | Permission and success |
| --- | --- | --- |
| POST `/v1/dms` | `{id, peer, commitment, envelope}` | Any member. 201, `{id}`. Returns 200 with the existing ID if the pair already has a DM |

`peer` is a different current person. Either person can create the DM, using a
random ID and an epoch 1 envelope addressed to their device. The normal channel
key flow wraps keys for both people's devices, including their later devices.
Both people can read and write regardless of role. DMs are visible only to the
two participants, including when an Owner, Admin or Moderator requests access.

The catalog includes DMs with `kind: "dm"`, `peer` set to the other person ID,
`category_id`, `name_ct` and `topic_ct` set to null, `position: 0`, and
empty `flags`. Clients display the peer's decrypted display name or person
fingerprint. Channel management routes cover text and voice channels.
When either person leaves the server through membership removal, the API
closes the pair's DM and deletes its history and envelopes. They can create a
new DM if the former member rejoins. Copies already held by either person can
remain on their devices.

## Messages

These routes require a bearer header and apply to text channels, voice channels and DMs.
IDs are random 32-byte client-generated values. A page is newest first by
insertion order. Pass the last record ID as `before` for the next page. If
that cursor was pruned or deleted, the API returns `404 message_not_found`
and the client can restart from the newest page.

| Method and path | Body or query | Permission and success |
| --- | --- | --- |
| GET `/v1/channels/{channel}/messages?before={id}&limit={n}` | Optional cursor and limit, 1–100, default 50 | `read`. 200, array of message records |
| POST `/v1/channels/{channel}/messages` | `{id, epoch, envelope}` | `write`. 201, the stored message record |
| GET `/v1/channels/{channel}/messages/{id}` | Empty | `read`. 200, one message record |
| DELETE `/v1/channels/{channel}/messages/{id}` | Empty | The author or a reader with `delete_messages`. 204 |
| GET `/v1/channels/{channel}/message-devices/{device}/{person}` | Empty | `read`. 200, the public device certificate as a base64url JSON string, when the device authored a retained message here |

A message record has `{id, epoch, person, sender_device, at, size, envelope}`.
`at` is the server's Unix timestamp. `size` is the binary envelope length.
`envelope` carries a one-byte version, 24-byte nonce, two-byte big-endian
ciphertext length, ciphertext including its authentication tag, and 64-byte
device signature. The POST limit is 16 KiB of JSON. The API verifies the
envelope against the authenticated person and device and requires the current
channel epoch. Rotation or a stale epoch returns `409 epoch_changed`, a
duplicate retained ID returns `409 message_exists`, and malformed framing or
signature returns `400 invalid_message`. IDs stay reserved after a delete or retention drop, and reusing one also returns
`409 message_exists`. Access failures use
`403 channel_read_denied` or `403 channel_write_denied`.

The API keeps the newest 10,000 records per channel. Explicit deletes remove
the ciphertext and add a `message_deleted` audit entry with actor, message ID
and time. DM audit entries are visible to participants with audit access. When the DM
closes, its entries become visible to everyone with audit access.
Retention drops also emit a delete event.

An edit is a new record with an encrypted `edit_of` reference. Clients
verify the original's signature, channel and author before accepting the edit.
Multiple edits can refer directly to the same original. If retention or a
delete removes the original, clients suppress edits that still reference that
ID. Deletes apply per stored record, because the API cannot see which records
are edits. When a client deletes a message, including a moderator delete, it
also deletes every edit of it that it has opened or sent.

## Voice

These routes require a bearer header and channel `read` access. They apply
to voice channels only.

| Method and path | Body | Success |
| --- | --- | --- |
| POST `/v1/voice/{channel}/token` | Empty | 200, `{token}`. LiveKit JWT valid for ten minutes for this room |
| GET `/v1/voice/{channel}/participants` | Empty | 200, `{people, unknown}`: sorted unique person IDs and a count of unmatched devices |

The room name is the encoded channel ID. The token's identity is
base64url of SHA-256 over `"rusttalk-voice-device-v1" || server_id ||
device_signing_public_key`, with each ID as 32 raw bytes. It carries
no display name or metadata. It permits subscription. Data publication
requires channel `write`. Clients consume byte streams only through the
independently authenticated live file protocol and drop other data events.
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

A screen or window is published as `screen_share` with VP8 simulcast and
the same epoch-bound LiveKit GCM frame encryption as audio. Screen labels
use the app audio wire format with the distinct HKDF info and associated
data prefix `rusttalk-screen-track-name-v1`. Receivers display “Shared
screen” if label decryption fails. The desktop portal supplies no title,
so Linux uses “Shared screen” or “Shared window” based on the source kind.
A camera is published as `camera` with VP8 simulcast and the same frame
encryption. Its LiveKit track name is the fixed string `camera`, so the
device name never leaves the sender; receivers show “Camera”. Clients
offer 480p, 720p or 1080p at 15 or 30 fps and open the camera's closest
mode. App audio remains a separate publication. Clients join with automatic
subscription disabled, subscribe to encrypted audio when announced, and
subscribe to each encrypted video track only while the viewer chooses it:
a watched screen, or a camera while its person's tile is on screen and
the viewer has not turned that person's video off.

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

## Live events

Open one `GET /v1/events` WebSocket per connection with the bearer
Authorization header. Use a query-less URL. The server sends JSON text
frames. It sends the outstanding key work at connection time and checks
session validity before pushing updates.

| Type | Other fields |
| --- | --- |
| `keys_needed` | `scope`, `epoch`, `device`. Reconcile pending wraps and delivered keys. Completion also wakes the recipient |
| `rotate_needed` | `scope`. Fetch the current epoch and try to publish its successor |
| `member_changed` | `person`, optional `device`. Refresh the member and device directory and catalog, and surface fingerprints |
| `channel_changed` | None. Refresh the catalog |
| `voice_changed` | `channel`. Fetch current voice participants. Sent to current channel readers on signed LiveKit join, leave and room-end notices |
| `dm_created` | `channel`. Refresh the catalog and reconcile keys |
| `message_created` | `channel`, `id`. Fetch the ciphertext record |
| `message_deleted` | `channel`, `id`. Remove the record and hide edits that reference it |
| `live_file_accepted` | `channel`, accepting `device`, signed `acceptance` object. Sent only to the offering device while it has channel read access |
| `resync` | None. The socket fell behind. Fetch current state, including keys, catalog, member devices and visible message history |

The API closes the socket when the token expires or ends, including kick,
ban and device revocation. Close reasons are fixed strings and logged.
The server keeps lagging sockets open and recomputes the outstanding
key work. DM, message and voice notices are sent only to current channel readers.
Fetch the roster when loading a channel list and after `resync`.

## Files

### Live transfer

A live offer is an original signed, encrypted chat message in the voice
channel. Its attachment list has count `1` and kind byte `1` (`live`).
The attachment payload in order is a random 32-byte transfer ID, a random
32-byte file key, the plaintext length (u64), the chunk size (u32, 65,536),
the filename byte length (u16) followed by the UTF-8 basename (1–1,024
bytes), and the MIME type byte length (u8) followed by the ASCII type
(1–127 bytes). Integers are big endian. The sender guesses the MIME type
from the extension and uses `application/octet-stream` when unknown.
Messages with zero attachments retain their original encoding.

The receiver chooses a destination and explicitly accepts an offer from a
currently verified device in the same voice room. It sends
`POST /v1/voice/{channel}/live-file` with a bearer header and JSON
`{recipient, transfer, nonce, cancel, signature}`. This route requires
voice channel `read` and returns 204 after queuing a volatile
`live_file_accepted` event. `recipient` is the offerer's Ed25519 public
key. The transfer ID and receiver-generated random 16-byte nonce are
encoded in base64url without padding, as are the recipient and signature.
`cancel` is false to accept and true to cancel that acceptance.
Both the API and offerer verify the acceptance signature. The API checks
that both devices are active in the LiveKit room before relaying an
acceptance. Each offer can serve each accepting device once
and accepts new recipients for ten minutes or until the sender cancels
or leaves the room.

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

### Server-stored files

Written in W13.

## Sync

Written in W14.
