# Crypto

## Identity and devices

The OS random number generator supplies 256 bits of entropy for a 24-word
English BIP-39 phrase. The standard BIP-39 seed uses an empty passphrase.
HKDF-SHA256 extracts from the 64-byte seed with the default zero salt and
expands 32 bytes for each ASCII label.

| HKDF info | Output |
| --- | --- |
| `rusttalk root v1` | Ed25519 root signing seed |
| `rusttalk sync v1` | Symmetric device sync key |

The person ID is the 32-byte Ed25519 root public key. For display, take the
first eight bytes of SHA-256 of that key and render them in lowercase hex as
four groups of four digits, such as `f851-b917-0ea8-a575`. Verification uses
the full person ID.

Each new device draws independent Ed25519 signing and X25519 agreement
secrets from the OS. Adding or restoring a device from the recovery words
derives the same person ID and sync key and issues a new certificate for its
fresh device keys.

### Signed records

Fields are concatenated in the order shown. Domain prefixes are ASCII bytes
without a terminator. Both timestamps are unsigned 64-bit big-endian Unix
seconds. The root signs every byte preceding the 64-byte Ed25519 signature.
The verifier takes the full person ID separately.

| Record | Field order |
| --- | --- |
| Public device certificate | `rusttalk-device-cert-v2`, Ed25519 public key (32 bytes), X25519 public key (32 bytes), creation time (8 bytes), device-name commitment (32 bytes), signature (64 bytes) |
| Legacy device certificate | `rusttalk-device-cert-v1`, Ed25519 public key (32 bytes), X25519 public key (32 bytes), creation time (8 bytes), name length (1 byte), device name (1–64 UTF-8 bytes), signature (64 bytes) |
| Revocation | `rusttalk-device-revocation-v1`, device Ed25519 public key (32 bytes), revocation time (8 bytes), signature (64 bytes) |

The parsers require exact lengths and reject malformed records. The public
v2 certificate is the only form accepted by the API. Its name commitment
is SHA-256 over `rusttalk-device-name-v2 || salt || name`, with a random
32-byte salt. The keystore uses `rusttalk-device-local-v2 || public_certificate ||
salt || name` to preserve the name locally. Verification checks the
commitment. Older local keystore records still load. A v1 device needs to
enroll a new device using the recovery words before signing in to W06.
A device is revoked when a verified revocation for the same person matches
its certified Ed25519 public key. The signed timestamp records when the
revocation was issued. The match applies regardless of certificate time.

Each key-state reply can include current members' signed revocations. Clients
verify each signature against the full person ID and persist the record in
their profile, indexed by person and device. All server connections consult
that same append-only set. Reordering, omitting or replaying records cannot
undo a revocation, including after a restart. Only the local person, pinned DM peers and people with verified member
certificates qualify. Key sync takes at most 4,096 of those records per reply,
the person's own first, and skips the rest. The profile stores at most 64 device
records per person and 1,024 other people; the person's own records are always
kept. A record over a storage limit is skipped without failing the sync; earlier
records remain. The API returns at most 4,096 records: the reader's own first, then
removals of devices it registered or saw send, then the rest.
No clock or list version is
needed because enrollment always generates fresh device keys.

Once observed, a revocation excludes that device from space, channel and DM
key wraps, envelope senders, live-file acceptance and message verification
(including cached or archived certificates). Unseen older messages from that
device also fail verification: the delivery time cannot prove a signature
predates revocation. Previously displayed plaintext and previously shared keys
cannot be recalled. On protocol 3 or later, a client that knows about a removal requires a fresh epoch for the
person's readable scopes, including DMs and voice channels. A sibling waits
0–249 ms and other readers wait 250–1,249 ms, then fetch again. An online
reader proposes a random key if the current epoch still lacks the exclusion.
The API accepts only the next epoch and verifies a new root-signed removal
for a scope reader before allowing an unscheduled rotation. Replays cannot
authorize another advance. A server refusing rotation can prevent progress.
Other clients drop tracks from the
revoked device's server-specific voice identity.
A fresh profile or peer to which every source withholds a record cannot know
about it. Older servers can omit the field and older clients ignore it.

### Local identity

One OS keystore entry holds both device secrets, the sync key, the person ID
and the public certificate. Its username is `identity-v1` and its service is
`RustTalk` or `RustTalk-dev-<p>` for a development profile. The entry contains
the 32-byte Ed25519 secret, 32-byte X25519 secret, 32-byte sync key, 32-byte
person ID and the certificate bytes in that order. On load the client verifies
the signature and checks both device secrets against the certificate.

Where these bytes live is the profile's key storage. `keystore.json` records
`storage` (`keychain`, `file` or `password`), `keys_saved` (an account's keys
were saved and not signed out) and `cleanup` (places whose copy is not the
profile's keys and is still to be removed):

- **Keychain** (the default when one is reachable): the OS keystore entry above.
- **Protected file:** `identity.key` in the profile folder, `RTKEYP01` followed
  by the same bytes, not encrypted. Files are written to a temporary file named
  `.identity.key.tmp-*` (mode 0600 on Unix), flushed, renamed over the old one,
  and the folder is flushed. Leftover temporary files are removed at start.
- **File with password:** `identity.key` holds `RTKEYE01 || m_cost || t_cost ||
  p_cost || salt || nonce || ciphertext`. The costs are big-endian u32 values
  (64 MiB, 3 passes, 1 lane when written today) and the salt is 16 random
  bytes. Costs outside 8 MiB to 1 GiB, 1 to 10 passes and 1 to 4 lanes are
  refused before any work, because the header is read before it is
  authenticated. Argon2id over the password with that salt and those costs
  gives a 32-byte XChaCha20-Poly1305 key, in a block buffer that is zeroed
  afterwards; the 24-byte nonce is fresh per write and the 36-byte header is the
  associated data, so changed costs fail like a wrong password. A file with
  weaker costs than today's is locked again with today's after unlocking. The
  derivation runs off the window's thread. The password and derived key are only
  held in memory, and the password fields' egui state is removed once a screen
  is done with them.

Phones never use the protected file. iOS keeps the same bytes in the
data-protection Keychain (generic password, service as above, account
`identity-v1`, `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`, the app's default
access group: never in iCloud Keychain, and an encrypted backup restores it only
to the same iPhone) and falls back to the file with password when the Keychain refuses
a test write. Android Keystore holds a non-exportable AES-256-GCM key under
`<service>/identity-v1`. The app prefers StrongBox when available, otherwise it
uses the regular Android Keystore provider. The provider chooses a fresh 12-byte
IV for every write. The private, backup-excluded `identity.wrapped` holds the IV,
ciphertext and 16-byte tag. A disposable alias tests encryption and decryption
before the app relies on Keystore. The key requires neither user authentication
nor an unlocked screen, so a lock during background voice cannot prevent a
key-storage operation. Anyone controlling the running app can use the key.
Sign-out deletes both the alias and the file. Android removes aliases on
uninstall. Without the alias, the recovery words are needed.

At start the client looks in both places: the chosen one first, then the other.
Keys found anywhere are used and `keystore.json` is repaired, so a lost or
changed choice never hides them. Each file says its own format, so a switch
between the two file kinds that is cut short still loads. With no keys found,
a profile with `keys_saved` (or, without `keystore.json`, with `servers.json` or
`sync.json`) shows a recovery screen and creates no new identity; when the keys
were in an unreachable keychain it points to Desktop Mode. Otherwise a missing
keychain leads to the choice between the two file kinds.

Switching between the keychain and the file first lists the new place under
`cleanup`, writes and reads back the new copy, then records the new choice with
the old place under `cleanup`, and removes the old copy. A failure before the
choice is renamed into place removes the new copy. A failure after the rename
keeps both copies, and the next start removes whatever `cleanup` lists, so a
crash at any point keeps one complete copy. Sign-out removes the keychain entry,
`identity.key` and temporary files whatever the choice; an entry it can't reach
stays under `cleanup` and is retried at each start. A forgotten password is
recovered by adding the device again with the recovery words.

What the files protect against: mode 0600 (Unix) and the per-user AppData
folder (Windows) keep other normal users out. They don't stop administrators,
root, SYSTEM or malware running as the same user. Development profiles under
`.dev/profiles/` lack the AppData guarantee on Windows. The protected file is
not encrypted, so a copy of it (a backup, a stolen disk) holds the keys. The
password file can be guessed offline, slowed by Argon2id, and protects nothing
once RustTalk is running and unlocked.

The recovery words and root signing key are never stored. Adding or revoking
a device requires the recovery words again. Settings lists other verified devices
from joined servers by their signing-key fingerprint. Removal signs one record
with the re-derived root and saves it before attempting device sync. The client records the person
fingerprint in the startup log.

### Device sync

Devices encrypt a JSON object containing settings and contacts keyed by field
and joined server entries with their pins. Contacts hold the friend list, keyed
by hex person ID (see [Protocol](protocol.md#friends)). XChaCha20-Poly1305 uses the
32-byte identity sync key and a fresh 24-byte OS-random nonce for each
version. Associated data is the ASCII bytes `rusttalk-sync-v1` followed by
the eight-byte big-endian version from the header. This authenticates the
version along with the contents. Devices can send the resulting blob
unchanged to every member server. The owner's signed device revocations travel in the encrypted blob. On sync,
the app verifies and unions them across all available snapshots and its local
records, then publishes its own records to each connected member server.
Disconnected servers catch up when they reconnect. Server-side device revocation blocks new
requests from that device. A revoked device can still decrypt copies it
already holds using its copy of the sync key.

## Server identity and sessions

The API creates a 32-byte Ed25519 signing seed with the OS random number
generator on first start and saves it as `server.key` (mode 0600) on the API
data volume. The 32-byte public key is the server ID. The human-readable
fingerprint uses the same SHA-256 truncation and formatting as a person ID.
The key signs `challenge || "rusttalk-server-v1"`. Clients verify both this
proof and their TLS trust before sending device credentials. Self-signed TLS
uses an explicit DER certificate pin in addition to the application key pin.

Challenges expire in 60 seconds and are deleted on consumption. The API
verifies device certificates in memory and checks stored revocations before
minting a session. A token is 32 bytes from the OS random number generator.
SQLite stores the SHA-256 digest as the session credential, with the person and
device binding and expiry. The client sends the encoded token in the HTTPS
Authorization header. Refresh replaces the token inside a transaction. An
invite code is 16 random bytes, hashed with SHA-256 before storage. The API
records the verified public device certificate on sign-in so clients can
wrap keys for the device even if it goes offline. Each person can register
up to ten devices per server. Revoking a device frees its slot.

## Channel and space keys

Each server has a separate space scope, and each text, voice or DM channel
has a channel scope. Each epoch key is 32 independent OS-random bytes.
The owner creates space epoch 1 on claim and uploads a self-addressed
envelope. A channel creator creates channel epoch 1 and uploads its
self-addressed envelope with channel creation. The identity sync key is used
exclusively for device sync.

The local recent-view cache derives a separate 32-byte key from the device
agreement secret using HKDF-SHA256 with info `rusttalk local cache v1`. Files
use XChaCha20-Poly1305 with a fresh random nonce. Associated data binds the
cache domain, person, device, pinned server and file type (for a cached
avatar, icon or emoji, the hash of its owner and ciphertext hash). Only verified
message bodies and decrypted, decodable pictures enter this cache. Opening it requires the unlocked device
identity and honors the profile’s signed device removals.

The epoch creator stores a 32-byte commitment with the epoch. It computes
HMAC-SHA256 keyed by the epoch key over these bytes.

```text
"rusttalk-key-commit-v1" || server_id (32 bytes) || kind (1 byte) ||
scope_id (32 bytes) || epoch (8 bytes, big endian)
```

A DM epoch's commitment also names its two people, so a key made for one pair
can't pass as another pair's. The person IDs are sorted bytewise.

```text
"rusttalk-dm-key-commit-v1" || server_id (32 bytes) || kind (1 byte) ||
scope_id (32 bytes) || epoch (8 bytes, big endian) || person_low (32 bytes) ||
person_high (32 bytes)
```

The API stores and signs over the commitment without reading it, so it needs
no change. A receiving app checks the commitment against the people its pin
names. A DM epoch with the plain commitment, from an app before pair binding,
stays readable but is never trusted for sending or wrapped to anyone; the app
re-keys instead. Apps before pair binding reject DM keys with the new
commitment, so a DM works only once both people are on this version.

### Epochs after a device removal

Each new epoch retains the preceding epoch's excluded device signing keys.
For a nonempty set, its commitment is SHA-256 of the ASCII domain
`rusttalk-excluded-devices-v1`, the 32-byte ordinary or pair-bound DM
commitment, then each ASCII `base64url(person_id).base64url(device_signing_key)` in bytewise string
order followed by a zero byte. The envelope signature covers this commitment.
Recipients verify it with the opened key and the advertised exclusions.
Even a reader unaware of the original removal refuses to wrap this key to
a listed device. Earlier epochs keep their original commitments and keys.

On protocol 3 or later, clients use their verified removals and the visible channel permissions
or pinned DM pair to determine which exclusions they require for sending.
The profile records each verified removal and its affected scopes per server,
so withholding a member or changing a role after restart cannot undo the
requirement. The per-server file is bounded at 4,096 removals and 65,536
scope associations. At either limit clients log and skip additional entries.
They refuse an old key in those scopes if the server suppresses rotation.
New media uses the rotated channel epoch for voice, camera, screen and shared
audio. A call keeps its current key while waiting for a wrap. A confirmed removal that requires a server update ends the affected call.
Clients that do not support this commitment cannot open affected epochs.
A server can still hide a removal or a relationship that a device has never
observed, or partition clients. Data already delivered stays with its readers.

### Envelopes

Use [RFC 9180](https://www.rfc-editor.org/rfc/rfc9180.html) base mode
with DHKEM(X25519, HKDF-SHA256) (0x0020), HKDF-SHA256 (0x0001) and
ChaCha20-Poly1305 (0x0003). The recipient key is the X25519 public key
in its verified device certificate. Use empty HPKE AAD. HPKE info is the
following fixed-length concatenation.

```text
"rusttalk-envelope-v1" || server_id (32 bytes) || kind (1 byte: space=1, channel=2) ||
scope_id (32 bytes) || epoch (8 bytes, big endian) || recipient_Ed25519 (32 bytes)
```

The 144-byte envelope is the 32-byte HPKE encapsulated public key, a
48-byte HPKE ciphertext and a 64-byte Ed25519 signature. The sender
signs the HPKE info followed by the epoch commitment, encapsulated key
and ciphertext. This signature binds the commitment when the API creates
the epoch and verifies later envelopes against the stored commitment.
The API checks signature, record size and sender and recipient access
before accepting an envelope. On receipt, clients verify the sender's
root-signed certificate in the current member directory and verify
the contextual envelope signature before opening it. They verify the opened
key against the commitment before using it. A device can reject a bad
envelope to reopen its slot. The API excludes that sender's person from
refilling the same recipient's slot for that scope and epoch while the
sender remains a member.

Clients holding a key respond to pending wraps for every eligible
device and epoch. Space keys go to all member devices. Channel keys
go to every device of a current reader. A kick, ban or device revocation
requests rotation of the space scope and the affected channel scopes.
Losing channel read access requests rotation of that channel. A remaining
reader publishes a new random epoch with a self-addressed envelope
in one transaction. Other devices get their own envelopes through the
same pending-wrap flow. A competing publisher discards its proposed key
and fetches the winning epoch. Historical ciphertext retains its epoch.
A historical key requires a remaining holder when a new envelope must
be created. If all holders lose a key, an old envelope whose sender is
no longer current cannot restore that history.

### Metadata

Use XChaCha20-Poly1305 with the space epoch key and an independent
random 24-byte nonce. Store the 8-byte big-endian epoch, nonce, and
ciphertext (including the 16-byte tag). Epochs are positive signed
64-bit values in both formats. Associated data is the following
concatenation.

```text
"rusttalk-meta-v1" || server_id (32 bytes) || object_type (1 byte) ||
object_id_length (1 byte) ||
object_id || epoch (8 bytes, big endian)
```

The object type values are server name (1), description (2), icon (3),
channel name (4), topic (5), category name (6), role name (7), display
name (8), avatar (9), server emoji or sticker name (10), server emoji
or sticker picture (11), role display (12), profile bio (13), emoji pack name (14)
and system message destination (15). Server objects use the raw server ID. Channels
and categories use their raw 32-byte IDs. Members (display name, avatar and bio) use the raw person ID.
Server emoji, stickers and emoji packs use their raw 32-byte IDs.
Role names use the role ID's UTF-8 bytes. New metadata uses the newest
space epoch held by the client, and the API accepts writes only when
that epoch is current and the scope is ready. A removed member who
colludes with the server can forge metadata under any old space key
they held. Clients prefer metadata from the newest epoch and re-encrypt
their own display name after a space rotation. Avatar, icon and emoji picture
ciphertext is capped at 256 KiB including the 48-byte encryption framing and tag. Clients
decode PNG, JPEG and GIF with pure Rust decoders, a 512×512 canvas limit,
up to 100 GIF frames and a 32 MiB limit on retained RGBA frame pixels.

### Invite previews

The client generates an independent random 32-byte link key. It seals
`epoch || space_key` (8 + 32 bytes) with XChaCha20-Poly1305 under the
link key, using a fresh 24-byte nonce and associated data
`rusttalk-invite-v1 || server_id`. The uploaded 80-byte blob is nonce
followed by ciphertext and tag. The invite also holds a server name
snapshot encrypted under that same space epoch. The link key appears
only in the invite fragment. After rotation an invite can still preview
its original snapshot. Once the invitee joins, it obtains readable
epochs through the normal envelope flow.

## Server events

An event is a format byte (1), a random 24-byte nonce, the XChaCha20-Poly1305
ciphertext with its tag under the space epoch key, and a 64-byte Ed25519
device signature. It is at most 3,246 bytes. The associated data is

```text
"rusttalk-event-v1" || server_id (32 bytes) || event_id (32 bytes) ||
epoch (8 bytes, big endian) || remove_at (8 bytes, big endian) ||
author_device (32 bytes) || author (32 bytes)
```

The signature covers the associated data, then the envelope without the
signature. The domain differs from messages and metadata, so none can pass
as another. The plaintext is the start and end (8 bytes each, Unix seconds,
big endian), the place, the title's length (2 bytes, big endian), the title (1 to 80
characters) and the description (up to 500 characters), both UTF-8. The place is
one byte: 0 for none; 1 followed by a 2-byte big-endian length and up to 200
characters of UTF-8 for a place in real life; or 2 followed by a 32-byte
channel ID. Apps show a channel only by the name they can already see.

`remove_at` must be the end plus two hours. The server needs it in the clear
to delete events, so it learns the end but not the start. Readers verify the
author's root-signed certificate sent with the record, refuse devices they
know were removed, verify the signature, decrypt and then check `remove_at`
against the signed end. An event that fails any check shows as unreadable
and can't be approved from that app. Approval itself is not signed: like
roles, the server decides it, so a malicious server can show a suggestion as
approved, hide events or replay a deleted one. It can't change one or make
one up in someone's name.

An answer to an event (going or not going) has the same framing: format 1, a
random nonce, the sealed answer byte (1 going, 2 not going) with its tag under
the space epoch key, and the device signature, 106 bytes in all. Its associated
data is `"rusttalk-event-answer-v1" || server_id || event_id || epoch (8 bytes,
big endian) || device || person`. Every answer has the same size, so the server
learns who answered but not what. Readers verify it like an event.

## Messages

Formats 2 and 3 (the envelope's first byte) have this plaintext, in order.

| Field | Encoding | Maximum bytes |
| --- | --- | --- |
| Flags | Bit 0 reply-to, bit 1 edit-of, bit 2 live offer, bit 3 sticker, bit 4 friend message, bit 5 key link, bit 6 poke. In v3 only, bit 7 quoted author | 1 |
| Kind | v3 only: 0 for an ordinary message, or a poll part (1 poll, 2 vote, 3 end) or voice request (4) followed by its fields, below. Other kinds are rejected | 1 + 4,030 |
| Text | Two-byte big-endian UTF-8 length, then text of at most 2,000 Unicode scalar values | 2 + 8,000 |
| Reply ID | 32 bytes, present with bit 0 | 32 |
| Quoted author | Raw 32-byte person ID, present with bit 7. Requires bit 0 and forbids bit 1 | 32 |
| Original ID | 32 bytes, present with bit 1 | 32 |
| Sticker ID | 32 bytes, present with bit 3 | 32 |
| Friend kind | One byte, present with bit 4: request (1), accept (2), decline (3), cancel (4), remove (5) | 1 |
| Friend send time | Eight-byte big-endian sender clock in milliseconds, present with bit 4 | 8 |
| Key link | Present with bit 5: the next epoch (eight-byte big-endian, positive) and its 32-byte commitment | 40 |
| Stored files | One-byte count 0–10, then that many [references](protocol.md#files) of at most 529 bytes | 1 + 5,290 |
| Live offer | Present with bit 2, [offer fields](protocol.md#live-transfer) | 460 |

A sticker message has no stored files and no live offer; its text is the
sticker's name, shown when the sticker is not available. Server emoji travel
in the text as `<:name:id>` tokens (see [Protocol](protocol.md#server-emoji-and-stickers)).
A friend message sets bit 4 alone, has empty text and no stored files, so its
plaintext is exactly 13 bytes; readers reject anything else with bit 4 or an
unknown kind (see [Protocol](protocol.md#friends)). A key link sets bit 5
alone, with empty text and no stored files (44 bytes). A poke sets bit 6
alone; its text is the poke's note, at most 100 Unicode scalar values and
possibly empty, with no stored files (see [Protocol](protocol.md#pokes)).
The sizes above are format 2's; format 3 adds the kind byte.

A poll part has no flags and no stored files (see
[Protocol](protocol.md#polls)). A poll's text is its question (1–300 Unicode
scalar values, not blank); its fields are a multiple-answers byte (0 or 1), an
eight-byte big-endian end time in unix seconds (0 for none), and a one-byte
count of 2–10 options, each a two-byte big-endian UTF-8 length and 1–100
Unicode scalar values, not blank. A vote has empty text; its fields are the
poll's 32-byte message ID, an eight-byte big-endian sender clock in
milliseconds, and a one-byte count of 0–10 option indexes, each one byte below
10, strictly ascending. An end has empty text; its field is the poll's message
ID. Format 2 can't carry a poll part. A reaction is a vote with 0 or 1
choices whose poll ID names the message and emoji (see
[Protocol](protocol.md#reactions)).
Apps before this format show any of these as a message they can't open.

A voice request (kind 4) has no flags, empty text and no stored files (46
bytes of plaintext for an ask, 38 for an answer). Its fields are an action byte and 32 bytes: action 0 asks
someone to join and holds the voice channel's ID followed by signed 64-bit
big-endian Unix seconds when the sender asked; 1 (joining), 2 (declined)
and 3 (coming later) answer and hold the request's message ID. Readers reject
other actions, and format 2 can't carry one (see
[Protocol](protocol.md#voice-requests)).
Nothing may follow the last field. The largest plaintext is 13,819 bytes
and the largest envelope is 13,926 bytes. Apps up to 0.8.1 accept at most four
stored files and show a message with more as one they can't open. Every length is checked against
its field maximum before reading. Readers reject version 1 envelopes.
Spaces and line breaks are preserved.

File names, for stored and live files, are 1–255 UTF-8 bytes. They are not
`.` or `..` and contain no `/`, `\`, `:`, control characters or
bidirectional formatting characters (U+061C, U+200E, U+200F, U+202A to
U+202E, U+2066 to U+2069). Senders replace such characters with `_` and
shorten longer names, keeping a short extension.

Use XChaCha20-Poly1305 with the current channel epoch key and an independent,
random 24-byte nonce. Associated data is the following concatenation.

```text
"rusttalk-message-v1" || server_id (32 bytes) || channel_id (32 bytes) ||
epoch (8 bytes, positive signed integer, big endian) || message_id (32 bytes) ||
sender_device_Ed25519 (32 bytes) || sender_person (32 bytes)
```

The sender signs the associated data followed by the envelope version byte, the nonce, the
two-byte big-endian ciphertext length, and the ciphertext including the
16-byte authentication tag. The signature is the final 64 bytes of the
envelope. Integer fields and lengths are fixed-width. The API verifies the
signature and outer framing before storage. The sender person binding
prevents a certificate under a different root from relabeling a signed post.

Recipients find the sender in the current verified device directory or the
channel's archived sender directory for retained history. They verify the
root certificate, the matching device and person IDs, the device signature,
and the AEAD tag before accepting the plaintext. A client keeps the opened
bodies of the newest 10,000 IDs per channel in memory across socket
reconnects and token refreshes. Opening the same record again returns the
cached body, so scroll-back and resync work. A different record under a
cached ID is rejected as a replay. To accept an edit it independently authenticates and opens the
original in the same channel and checks that it has the same person ID.
The server supplies delivery timestamps and order.

### Direct messages on several servers

A DM has a copy on every server both people share. Each copy is an ordinary
DM channel with its own channel key and epochs, created and wrapped as above.
A message is sealed and signed separately for each copy under that server's
current DM epoch, with the associated data above. Only the random message ID
is shared, so readers can merge the copies by person and ID.

The associated data keeps the server and channel IDs rather than binding a
server-independent conversation ID. A server can then neither replay another
server's copy nor accept it, because the API verifies the signature against its
own server and channel. Each copy's key also follows that server's membership
and rotations, so a revocation or removal on one server rotates only that copy
and needs no key shared across servers. The wire format is unchanged and the
API needs no new checks. Encrypting per server costs one seal per copy.

Backfill verifies the author’s source record and checks that its epoch key
commits to the pinned pair before copying it. An older ordinary channel
relabeled as a DM cannot supply history for a different conversation.
The destination also needs its own trusted current DM epoch.

A sender could post different bodies under one ID to different servers. This
only affects the conversation partner, who could equally receive two separate
messages. Readers show one readable copy. Merging by person and ID stops one
participant's record from replacing the other's under a reused ID, and an edit
applies only to the original with the same person. Ordering uses server
timestamps, because the plaintext carries no send time; adding one would
change the plaintext format that current apps parse. Readers prefer the earliest ordinary server receipt time from readable copies.
Only when all readable copies are marked backfilled do they use the earliest
restored time. A restored time is supplied by the author, bounded by the
destination's clock; the marker is supplied by the server. Neither proves
when the original was sent. Server clocks can differ, so messages
sent within that difference can appear out of order. A dishonest server can
also date its copy early and move the message up in the history; the message
stays readable there.

Edits don't depend on server time. The author's app puts a revision in the
signed and encrypted edit (see [Protocol](protocol.md#messages)), so no server
can make an older edit win. The revision is the author's clock in
milliseconds, raised above the shown edit's revision. If the author edits
again from a device whose clock is behind, before that device has loaded the
latest edit, the earlier edit can stay shown. Edits from older apps carry no
revision and are still ordered by server time.

A dishonest server could try to route a DM into a channel a third person can
read: list a channel with that person as the DM, or ask honest apps to wrap
the DM key to that person's device. The app pins each DM channel to the person
it was first listed with and doesn't use it if the server relabels it. For a
pinned DM it accepts an epoch key only from, and wraps it only to, a
root-certified device of one of the two people, and it wraps a DM key only
when its commitment names the pinned pair. If a dishonest server gives two
devices of one person different pins for the same channel, a device can't
take a key made for the other pair, so it can't pass it on to the person its
own pin names. Asked for a DM it already has, the server may answer with a
channel pinned to someone else; the app then reports an error and doesn't
use that channel.

A key from before the pin may already be held by others, for example when the
channel was an ordinary channel before. An epoch number doesn't show that a
key is new: a dishonest server could hand out an older shared key under a
higher number, wrapped by one of the two people who received it earlier. So
the app trusts a DM epoch only through a chain of keys:

- **Anchor.** The epoch of a key this device generated itself after pinning
  the DM, with its commitment. A DM it creates is pinned before its first key
  is wrapped, so that epoch 1 is the anchor. The anchor is saved with the pin
  and moves up as the chain grows.
- **Link.** Before people start a DM's next epoch, their app sends a key link
  in the current epoch: the next epoch number and the new key's commitment,
  inside the signed, encrypted message. The app trusts epoch N when a link
  naming N, with N's actual commitment, was sent by one of the two people in
  an epoch it already trusts. A commitment can only be made with the key, so
  an older key under a new number doesn't match.
- **Re-key.** When the current epoch doesn't chain to the anchor, the app looks
  for the link in recent history (four pages). If there is none, it sends a
  link and starts the next epoch itself, wrapped only to the pair. A device
  that never got the current key (for example, both people on new devices)
  waits one round for it, then starts the next epoch without a link. Only that
  device trusts the unlinked epoch at first, because it made the key after
  pinning. The other person can't trust it, so their app re-keys from it with a
  link, which the first device then trusts. Nobody else's unlinked epoch is
  ever trusted.

The app sends a message, friend message or live-file offer to a DM copy only
when its current epoch is trusted this way and its key came from one of the
two people. In a DM it takes a live-file acceptance only from a certified
device of the pinned other person and accepts an offer only from one of the
two. A failed recovery (no link found, a re-key refused or raced) is tried
again 15 seconds later, then waiting twice as long each time (up to 16
minutes). The connection syncs at that time even without a server event, and
a message waiting to go out tries again at once. If the
pins can't be saved, the app pauses that server's DMs, live-file acceptances
included. If
the saved pins can't be read, the app blocks that server's DMs, including new
ones, instead of pinning them again from the server's catalog. Either person
can start a DM epoch at any time, so either can disrupt delivery by re-keying
over and over; that is accepted.

This relies on both people's apps. An older app still wraps the key, also a
re-keyed one, to any device the server names, so a dishonest server can still
read along with a peer on an older app. This remains a high risk until both
people update. A server without DM re-keying refuses the new epoch, and its DM
copies stay unused.

Friend messages are ordered by their signed send time, not the server's. If
someone's devices disagree on the time, a later friend message from the device
whose clock is behind can be ignored.

A record that fails to verify or decrypt never merges with a readable copy, so
a server can't use one to change a message's time or servers, use up its
notification, have the author's delete go to that server, or change friend
state. Deletes are not signed: a server's delete event removes only that
server's copy. Only the author's own delete of a readable copy, kept on the
author's devices and synced between them, removes the message from every
copy.

An archived sender is a device that is no longer in the current directory.
After membership removal, clients still verify its root-signed certificate
for retained history. A device that held an epoch key can sign new records
for that epoch. An honest API requires a current session and epoch. A peer
that has seen the owner's signed revocation also rejects those records when
a dishonest API backdates them or supplies the genuine archived certificate.
Membership removal alone provides no such signed proof.

## Server-stored file encryption

The sender generates a fresh random 32-byte key and independent random 32-byte
ID for each blob. PNG, JPEG, GIF and WebP files (first frame) that decode within the image limits
(4096 pixels per edge, 64 MiB of pixels) get a separate encrypted PNG thumbnail
with a longest edge of at most 320 pixels. Other files, including other image
formats, are sent without one. Receivers decode a thumbnail only as a PNG of at
most 320 pixels per edge. Thumbnail bytes have their own key
and ID. Keys, names, media types and plaintext framing travel in the signed,
encrypted message.

The sender splits the plaintext into 64 KiB chunks with a possibly shorter last
chunk. An empty file has one empty final chunk. Each chunk is independently
sealed with XChaCha20-Poly1305. Its 24-byte nonce is 16 zero bytes followed by
the zero-based chunk index as a big-endian unsigned 64-bit integer. The key is
used for exactly one blob, so every nonce under it is distinct. The associated
data is the following concatenation.

```text
"rusttalk-file-chunk-v1" || file_id (32 bytes) || index (8 bytes, big endian) ||
final (1 byte, 1 for the last chunk and 0 otherwise)
```

The blob concatenates each ciphertext and its 16-byte tag without headers.
The recipient uses the authenticated plaintext size and chunk size to enforce
the exact ciphertext length and framing. It verifies every tag before
returning bytes to the caller, so substitution, tampering, reordering and
truncation fail. The image decoder checks actual bytes and accepts only
pure-Rust PNG and JPEG decoding. It caps compressed input at 32 MiB and
sets `image::Limits` to 4,096 pixels per axis and a 64 MiB allocation budget.

A file shared to a channel's file list (see [Protocol](protocol.md#shared-files))
carries its reference in a format 3 envelope sealed with the channel key,
under the file's ID as the message ID. It is built and signed like a message,
but its associated data starts with `"rusttalk-shared-file-v1"` instead of
`"rusttalk-message-v1"`, with the same fields after it. So a server can't
replay a shared file's record as a chat message, or a chat message as a shared
file: the API and readers check each with its own label, and the signature and
tag fail under the other. Readers accept the record only when it holds that one
file, without a thumbnail, and nothing else.

## Media encryption

Before joining, RustTalk derives a 32-byte key with HKDF-SHA256 using
the current voice channel epoch key as input, the default zero salt and
the following ASCII label and binary context as info.

```text
"rusttalk-media-v1" || server_id (32 bytes) || channel_id (32 bytes) ||
epoch (8 bytes, positive signed integer, big endian)
```

The SDK uses AES-GCM frame encryption from the start of the room session.
The primary key slot is `epoch mod 16` and the SDK's automatic ratchet is
disabled. Each device reconciles the normal channel-key events, installs
the newer derived key and switches every local sender to its slot.
Installing an epoch writes its key into all 16 slots, which retires
every older key. Frames stamped with an older epoch's slot then fail
decryption and are dropped, so a removed sender that kept its old key is
not heard. The SDK's new or automatically republished senders start at
slot zero, which also holds the current key. Receivers use each frame's
stamped slot. A peer missing the new epoch drops those frames until its
envelope arrives, and a sender that has not installed the new epoch is
briefly silent to peers that have. LiveKit never
receives the media key. The LiveKit JWT authorizes signaling and does
not carry the media key.

## Trust model

The server decides membership and channel access. A malicious server
can add a person it controls, or misrepresent who has access, and induce
clients to wrap keys to that person's devices. A root signature binds
a known person's device keys. It cannot show that the person belongs
in a space or channel. Clients show observed joins and leaves in the configured text channel.
Members with audit access can inspect device changes and their fingerprints. The bot also logs the person
and device fingerprints for every wrap. The API never receives the
decryption keys from an honest client. Former readers retain anything
they already decrypted or saved. Rotation excludes them from subsequent
epochs while access is denied. A re-invited member receives the full
history for scopes they can read, including epochs created to exclude
them. Membership removal closes their existing DMs. Each commitment attests to the key chosen by the epoch creator.
If that creator publishes an unusable key, recovering requires a new
epoch.

The selected [hpke crate](https://github.com/rozbb/rust-hpke#security)
uses the RFC 9180 construction and is dual-licensed MIT/Apache-2.0.
The maintainers cite a Cloudflare security review of version 0.8 and
report no formal audit. RustCrypto's `chacha20poly1305` is also
dual-licensed MIT/Apache-2.0.

### What the server challenge signature does not cover

The server's signature over the challenge proves the server holds the pinned key, but it is not bound to the TLS connection and the client adds no nonce of its own. In self-signed mode the TLS certificate pin stops a relay. In CA mode (public domain, no certificate pin), an attacker holding a valid certificate for the domain could relay a sign-in to the real server. Binding the device signature to a TLS exporter value would close this; it is not done yet.


## Live files

Use a fresh random 32-byte XChaCha20-Poly1305 key for each offered file.
The key is inside the signed chat ciphertext. For each recipient, take
the first 16 bytes of SHA-256 over
`"rusttalk-file-nonce-v1" || recipient_device_Ed25519 || acceptance_nonce`,
using the raw 32-byte device public key and raw 16-byte nonce. Append the
chunk index as a big-endian u64 to form the 24-byte AEAD nonce. Each
device can accept a given offer at most once, so retries require a fresh
offer and file key.

Authenticate this associated data on each chunk.

```text
"rusttalk-file-chunk-v1" || transfer_id (32 bytes) ||
chunk_index (8 bytes, big endian) || final (one byte, 0 or 1)
```

The receiver derives the expected count and exact final length from the
signed offer. It rejects a missing or unauthenticated final chunk, a
reordered or spliced chunk, and any bytes after the final chunk. Chunk
authentication is independent of LiveKit's data encryption.

The accepting device signs the following with its Ed25519 device key.

```text
"rusttalk-file-accept-v1" || server_id (32 raw bytes) || channel_id (32 raw bytes) ||
recipient (43 ASCII bytes) || transfer (43 ASCII bytes) || nonce (22 ASCII bytes) ||
cancel (one byte, 0 or 1)
```

The recipient, transfer and nonce fields use their base64url wire
strings. The offering device verifies with the accepting device's
current root-certified public key. It uses the room identity derived
from that same key as the only stream destination. In a data-only room both
participant identities also include the acceptance nonce. The file encryption
and signed acceptance are the same for in-call and data-only transfers.
