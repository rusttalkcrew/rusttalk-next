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
| Device certificate | `rusttalk-device-cert-v1`, Ed25519 public key (32 bytes), X25519 public key (32 bytes), creation time (8 bytes), name length (1 byte), device name (1–64 UTF-8 bytes), signature (64 bytes) |
| Revocation | `rusttalk-device-revocation-v1`, device Ed25519 public key (32 bytes), revocation time (8 bytes), signature (64 bytes) |

The parsers require the exact encoded length and reject malformed records.
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
SQLite stores only its SHA-256 digest as the credential, with the person and
device binding and expiry. The client sends the encoded token in the HTTPS
Authorization header. Refresh replaces the token inside a transaction. An
invite code is 16 random bytes, hashed with SHA-256 before storage.

## Channel and space keys

Written in W06.

## Messages

Written in W07.

## Media encryption

Written in W08.

## Trust model

Written in W06.
