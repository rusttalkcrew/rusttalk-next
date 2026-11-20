# Protocol

The API runs over HTTPS. Requests with JSON bodies use `Content-Type: application/json`.
Each request body is limited to 4096 bytes. Binary fields use canonical unpadded
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
certificate uses the W04 format described in [Crypto](crypto.md). Use a fresh
challenge per attempt. The server consumes a recognized challenge before
verifying the device proof, and rejects challenges after 60 seconds. At most
256 challenges are outstanding across the server.

Members sign in with their device proof. For a new member, supply the invite
code as the optional `invite` field. Invite redemption, membership and session
creation are atomic. Each person and device pair can have one current session.
Successful sign-in replaces any previous token for the device. A refresh
rotates the bearer token and extends expiry to 12 hours from the refresh.

## Invite links and server pins

```text
rusttalk://<host>[:port]/i/<code>#<server_id>[.<tls_sha256>]
```

Use a hostname, IPv4 address or bracketed IPv6 address. Port defaults to 443.
The code is 16 random bytes. The fragment carries the full 32-byte Ed25519
server ID, followed in self-signed mode by the 32-byte SHA-256 hash of the DER
leaf certificate. Public CA mode omits the TLS field and uses normal TLS
verification. W06 can extend the fragment with another dot-separated field
for the space-key data. W05 clients accept only the fields above.

Pass a link through a trusted channel and give the HTTPS API only its code,
inside the sign-in body. Clients pin the verified server ID and any explicit
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
| POST `/v1/invites` | `{uses, ttl_seconds}` | `invite`. 201, `{id, code, expires_at}` |
| DELETE `/v1/invites/{id}` | Empty | `invite`. 204 |
| DELETE `/v1/members/{person}` | Empty | `kick`. 204 |
| PUT `/v1/bans/{person}` | `{ban_ip?}`, default false | `ban`. 204 |
| DELETE `/v1/bans/{person}` | Empty | `ban`. 204 |
| GET `/v1/roles` | Empty | Any member. 200, `{default_role_id, roles}` |
| POST `/v1/roles` | `{rank, permissions, name_ct?}` | `manage_roles`. 201, the created role |
| DELETE `/v1/roles/{id}` | Empty | `manage_roles`. 204 |
| PUT `/v1/roles/default` | `{role_id}` | `manage_roles`. 204 |
| PUT `/v1/members/{person}/roles` | `{role_ids}` | `manage_roles`. 204, replaces assigned roles |
| PUT `/v1/owner` | `{person_id}` | Owner. 204, transfers ownership to a member |
| GET `/v1/audit?after=0` | Empty | `view_audit_log`. 200, an array of up to 100 audit entries in ascending ID order |

A role has `{id, kind, rank, permissions, name_ct}`. A member's permissions
are the union of their role flags, and their highest rank is used for
hierarchy. Owner (1000) and Admin (100) have all six flags. Moderator (50)
has `kick`, `ban`, and `delete_messages`. Member (0) has no flags and starts
as the default. The other flags are `invite`, `manage_roles`, and
`view_audit_log`. `delete_messages` is reserved for W07. Built-in role IDs
match their kinds. Custom roles have opaque IDs and ranks from 1 to 999,
strictly below their creator. They can only receive flags that the creator
has. `name_ct` is null or at most 512 bytes of opaque ciphertext after
decoding. The server allows up to 128 roles including built-ins.
Deleting a custom role requires clearing its assignments and replacing it
as the default first. Changing the default affects future joins.

Kick, ban, and role assignment require the actor to outrank the target
strictly. Granting a role or choosing it as the default also requires the
actor to outrank that role and hold every permission it grants. The
owner role comes from a singleton ownership record and can only move via
the transfer endpoint. The prior owner keeps their assigned roles.

Invite uses may be 1–1000, with lifetimes from 1 second to 30 days. The
bootstrap invite is single-use and expires after a day. Kick and ban
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

## Channels and messages

Written in W06 and W07.

## Files

Written in W13.

## Sync

Written in W14.
