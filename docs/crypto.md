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
self-addressed envelope with channel creation. The identity sync key is
separate and reserved for device sync in W14.

The epoch creator stores a 32-byte commitment with the epoch. It computes
HMAC-SHA256 keyed by the epoch key over these bytes.

```text
"rusttalk-key-commit-v1" || server_id (32 bytes) || kind (1 byte) ||
scope_id (32 bytes) || epoch (8 bytes, big endian)
```

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
name (8), and avatar (9). Server objects use the raw server ID. Channels
and categories use their raw 32-byte IDs. Members use the raw person ID.
Role names use the role ID's UTF-8 bytes. New metadata uses the newest
space epoch held by the client, and the API accepts writes only when
that epoch is current and the scope is ready. A removed member who
colludes with the server can forge metadata under any old space key
they held. Clients prefer metadata from the newest epoch and re-encrypt
their own display name after a space rotation.

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

The plaintext contains a one-byte bit mask (bit 0 for reply-to, bit 1 for
edit-of), a two-byte big-endian UTF-8 byte length, the UTF-8 text, the optional
32-byte reply ID, the optional 32-byte original ID, and a one-byte attachment
count. A count of zero has no payload. A count of one is followed by the
live attachment described in [the protocol](protocol.md#live-transfer).
This version requires all other flag bits to be clear, valid
UTF-8 with at most 2,000 Unicode scalar values, and exact lengths. Spaces
and line breaks are preserved.

Use XChaCha20-Poly1305 with the current channel epoch key and an independent,
random 24-byte nonce. Associated data is the following concatenation.

```text
"rusttalk-message-v1" || server_id (32 bytes) || channel_id (32 bytes) ||
epoch (8 bytes, positive signed integer, big endian) || message_id (32 bytes) ||
sender_device_Ed25519 (32 bytes) || sender_person (32 bytes)
```

The sender signs the associated data followed by byte `1`, the nonce, the
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
from that same key as the only stream destination.
