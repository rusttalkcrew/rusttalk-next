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

### Local identity

One OS keystore entry holds both device secrets, the sync key, the person ID
and the public certificate. Its username is `identity-v1` and its service is
`RustTalk` or `RustTalk-dev-<p>` for a development profile. The entry contains
the 32-byte Ed25519 secret, 32-byte X25519 secret, 32-byte sync key, 32-byte
person ID and the certificate bytes in that order. On load the client verifies
the signature and checks both device secrets against the certificate.

The recovery words and root signing key are never stored. Adding or revoking
a device requires the recovery words again. The client records the person
fingerprint in the startup log.

### Device sync

Devices encrypt a JSON object containing settings and contacts keyed by field
and joined server entries with their pins. Contacts hold the friend list, keyed
by hex person ID (see [Protocol](protocol.md#friends)). XChaCha20-Poly1305 uses the
32-byte identity sync key and a fresh 24-byte OS-random nonce for each
version. Associated data is the ASCII bytes `rusttalk-sync-v1` followed by
the eight-byte big-endian version from the header. This authenticates the
version along with the contents. Devices can send the resulting blob
unchanged to every member server. Server-side device revocation blocks new
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
name (8), avatar (9), server emoji or sticker name (10), and server emoji
or sticker picture (11). Server objects use the raw server ID. Channels
and categories use their raw 32-byte IDs. Members use the raw person ID.
Server emoji and stickers use their raw 32-byte IDs.
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

## Messages

Format version 2 (the envelope's first byte) has this plaintext, in order.

| Field | Encoding | Maximum bytes |
| --- | --- | --- |
| Flags | Bit 0 reply-to, bit 1 edit-of, bit 2 live offer, bit 3 sticker, bit 4 friend message, bit 5 key link. Other bits clear | 1 |
| Text | Two-byte big-endian UTF-8 length, then text of at most 2,000 Unicode scalar values | 2 + 8,000 |
| Reply ID | 32 bytes, present with bit 0 | 32 |
| Original ID | 32 bytes, present with bit 1 | 32 |
| Sticker ID | 32 bytes, present with bit 3 | 32 |
| Friend kind | One byte, present with bit 4: request (1), accept (2), decline (3), cancel (4), remove (5) | 1 |
| Friend send time | Eight-byte big-endian sender clock in milliseconds, present with bit 4 | 8 |
| Key link | Present with bit 5: the next epoch (eight-byte big-endian, positive) and its 32-byte commitment | 40 |
| Stored files | One-byte count 0–4, then that many [references](protocol.md#files) of at most 529 bytes | 1 + 2,116 |
| Live offer | Present with bit 2, [offer fields](protocol.md#live-transfer) | 460 |

A sticker message has no stored files and no live offer; its text is the
sticker's name, shown when the sticker is not available. Server emoji travel
in the text as `<:name:id>` tokens (see [Protocol](protocol.md#server-emoji-and-stickers)).
A friend message sets bit 4 alone, has empty text and no stored files, so its
plaintext is exactly 13 bytes; readers reject anything else with bit 4 or an
unknown kind (see [Protocol](protocol.md#friends)). A key link sets bit 5
alone, with empty text and no stored files (44 bytes). Apps before this format
show either as a message they can't open.
Nothing may follow the last field. The largest plaintext is 10,644 bytes
and the largest envelope is 10,751 bytes. Every length is checked against
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

The sender signs the associated data followed by byte `2`, the nonce, the
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

A sender could post different bodies under one ID to different servers. This
only affects the conversation partner, who could equally receive two separate
messages. Readers show one readable copy. Merging by person and ID stops one
participant's record from replacing the other's under a reused ID, and an edit
applies only to the original with the same person. Ordering uses server
timestamps, because the plaintext carries no send time; adding one would
change the plaintext format that current apps parse. Readers use the earliest
timestamp seen for a readable copy. Server clocks can differ, so messages
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

Device revocation still follows the server's device list (issue #109). A
dishonest server can keep a revoked device eligible for future DM keys until
#109 is fixed. This predates DMs over several servers and applies to every
channel.

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

An archived sender is a device that is no longer in the current directory,
for example after revocation or removal of its person. Clients accept its
root-signed certificate for any epoch of retained history. Removal and
revocation rotate every affected scope, and honest clients wrap later epoch
keys only to current directory devices. A removed device therefore cannot
encrypt a valid message for a later epoch. It can still sign records for
epochs whose keys it held. An honest API refuses them because the device has
no session and the API accepts only the current epoch. A malicious API could
store such a back-dated record from a stolen, revoked device. Clients cannot
detect this, because the API also controls the device directory and does not
expose root-signed revocation records to other members. A removal epoch
reported by the API would add no protection under the same assumption.

## Server-stored file encryption

The sender generates a fresh random 32-byte key and independent random 32-byte
ID for each blob. PNG and JPEG files that decode within the image limits
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
in a space or channel. Clients surface every observed member and device
change with person and device fingerprints. The bot also logs the person
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
