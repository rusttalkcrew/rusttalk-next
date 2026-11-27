//! Authentication wire records, signed bytes and server pins.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::{
    identity::{DeviceIdentity, IdentityError},
    keys::Key,
};

pub type ServerId = [u8; 32];
pub type Fingerprint = [u8; 32];
pub type InviteCode = [u8; 16];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("invalid base64url encoding or byte length")]
    Encoding,
    #[error("invalid RustTalk invite link")]
    Invite,
    #[error("server identity changed at the known address")]
    ServerIdChanged,
    #[error("server TLS certificate fingerprint changed at the known address")]
    TlsChanged,
    #[error("invalid server challenge signature")]
    ServerProof,
}

pub fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Opaque, server-specific LiveKit identity for a device signing public key.
pub fn voice_identity(server: &ServerId, device: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"rusttalk-voice-device-v1");
    hash.update(server);
    hash.update(device);
    encode(&hash.finalize())
}

/// LiveKit identity scoped to one acceptance, so transfers can run concurrently.
pub fn live_file_identity(server: &ServerId, device: &[u8], nonce: &[u8; 16]) -> String {
    format!("{}.{}", voice_identity(server, device), encode(nonce))
}

pub fn decode_bytes(encoded: &str) -> Result<Vec<u8>, ProtocolError> {
    URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| ProtocolError::Encoding)
}

pub fn decode<const N: usize>(encoded: &str) -> Result<[u8; N], ProtocolError> {
    let mut bytes = [0; N];
    if URL_SAFE_NO_PAD.decode_slice(encoded, &mut bytes).ok() != Some(N) {
        return Err(ProtocolError::Encoding);
    }
    Ok(bytes)
}

#[derive(Serialize, Deserialize)]
pub struct Challenge {
    pub challenge: String,
    pub server_id: String,
    pub signature: String,
}

#[derive(Serialize, Deserialize)]
pub struct SignIn {
    #[serde(default)]
    pub protocol: u32,
    pub person_id: String,
    pub certificate: String,
    pub challenge: String,
    pub signature: String,
    pub invite: Option<String>,
}

pub const PROTOCOL_VERSION: u32 = 3;

#[derive(Serialize, Deserialize)]
pub struct ServerInfo {
    #[serde(default)]
    pub dm_backfill: bool,
    pub open_joining: bool,
    pub version: Option<String>,
    pub protocol: Option<u32>,
    /// Stored files a message may carry here; servers that leave it out take four.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files: Option<u8>,
}

impl ServerInfo {
    /// Stored files per message this server accepts, within what this app can send.
    pub fn max_files(&self) -> usize {
        use crate::messages::{LEGACY_MAX_ATTACHMENTS, MAX_ATTACHMENTS};
        self.max_files.map_or(LEGACY_MAX_ATTACHMENTS, |max| {
            (max as usize).clamp(LEGACY_MAX_ATTACHMENTS, MAX_ATTACHMENTS)
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileLimits {
    pub max_file_bytes: i64,
    pub member_storage_bytes: i64,
    pub server_storage_bytes: i64,
    pub retention_secs: i64,
    pub max_stickers: i64,
}

impl FileLimits {
    /// The highest limits RustTalk can work with; the server reports them as its
    /// ceilings when its host set none. A file stays within one attachment's
    /// ciphertext, and retention stays far below where `created_at + retention`
    /// overflows or reaches the never-expires marker (about 2,700 years).
    pub const MAX: Self = Self {
        max_file_bytes: 32 * 1024 * 1024,
        member_storage_bytes: i64::MAX,
        server_storage_bytes: i64::MAX,
        retention_secs: 1_000_000 * 86_400,
        max_stickers: i64::MAX,
    };
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileSettings {
    pub limits: FileLimits,
    pub ceilings: FileLimits,
    pub used_bytes: i64,
}

/// How many of the newest messages each channel keeps (`/v1/settings/message-history`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHistory {
    /// 0 keeps every message.
    pub keep: i64,
    /// The most the server's host allows, when it set a cap (`RUSTTALK_MESSAGE_RETAIN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<i64>,
}

/// How fast each member may act before the server asks them to slow down (429).
/// Ordered from strictest to loosest, so a host cap is the smaller of the two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitPreset {
    Strict,
    #[default]
    Standard,
    Relaxed,
}

/// The server's rate limits (`/v1/settings/rate-limits`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimits {
    pub preset: RateLimitPreset,
    /// The loosest preset the server's host allows, when it set one (`RUSTTALK_RATE_LIMITS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<RateLimitPreset>,
}

/// Server statistics (`GET /v1/stats`): counts and sizes only, never content, names or
/// addresses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerStats {
    pub version: String,
    pub started_at: i64,
    pub uptime_secs: u64,
    /// People with the app open now.
    pub connected: u64,
    pub peak: StatsPeak,
    pub members: i64,
    /// Sign-ins since start.
    pub logins: u64,
    /// Messages sent since start.
    pub messages: u64,
    pub traffic: StatsTraffic,
    pub storage: StatsStorage,
    /// Resident memory of the API, where the system tells it (Linux).
    pub memory: Option<u64>,
}

/// Most people connected at once since start, and when.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsPeak {
    pub count: u64,
    pub at: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsTraffic {
    /// The API's own connections: sign-in, chat, files and live updates.
    pub control: StatsDirection,
}

/// Bytes since start and per second just now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsDirection {
    pub received: u64,
    pub sent: u64,
    pub received_per_sec: u64,
    pub sent_per_sec: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsStorage {
    /// The database with its write-ahead log.
    pub database: u64,
    pub files: u64,
}

/// New member rules (`/v1/settings/new-member-rules`): for `secs` after joining, members
/// with only the default role can't upload files, start DMs, mention everyone or roles, or
/// create invites. 0 is off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMemberRules {
    pub secs: i64,
}

impl NewMemberRules {
    /// The longest period: 30 days.
    pub const MAX_SECS: i64 = 30 * 86_400;
}

/// Operational update status visible to the server owner.
#[derive(Default, Serialize, Deserialize)]
pub struct ServerUpdates {
    pub pending: Option<PendingServerUpdate>,
    pub last: Option<ServerUpdateResult>,
}

#[derive(Serialize, Deserialize)]
pub struct PendingServerUpdate {
    pub version: String,
    pub since: i64,
}

#[derive(Serialize, Deserialize)]
pub struct ServerUpdateResult {
    pub version: String,
    pub at: i64,
    pub outcome: ServerUpdateOutcome,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerUpdateOutcome {
    Updated,
    Failed,
}

#[derive(Serialize, Deserialize)]
pub struct OpenJoining {
    pub open_joining: bool,
    pub member_cap: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub expires_at: i64,
}

impl Challenge {
    pub fn signed(challenge: &[u8; 32], key: &SigningKey) -> Self {
        Self {
            challenge: encode(challenge),
            server_id: encode(key.verifying_key().as_bytes()),
            signature: encode(&key.sign(&server_payload(challenge)).to_bytes()),
        }
    }

    pub fn verify(&self, server_id: &ServerId) -> Result<[u8; 32], ProtocolError> {
        let challenge = decode(&self.challenge)?;
        let signature = Signature::from_bytes(&decode(&self.signature)?);
        if decode::<32>(&self.server_id)? != *server_id {
            return Err(ProtocolError::ServerIdChanged);
        }
        VerifyingKey::from_bytes(server_id)
            .and_then(|key| key.verify_strict(&server_payload(&challenge), &signature))
            .map_err(|_| ProtocolError::ServerProof)?;
        Ok(challenge)
    }
}

impl SignIn {
    pub fn signed(
        identity: &DeviceIdentity,
        challenge: &[u8; 32],
        server_id: &ServerId,
        invite: Option<&InviteCode>,
    ) -> Result<Self, IdentityError> {
        Ok(Self {
            protocol: PROTOCOL_VERSION,
            person_id: encode(identity.certificate.person_id()),
            certificate: encode(identity.certificate.public_bytes()?),
            challenge: encode(challenge),
            signature: encode(
                &identity
                    .signing_key
                    .sign(&auth_payload(challenge, server_id))
                    .to_bytes(),
            ),
            invite: invite.map(|code| encode(code)),
        })
    }
}

fn server_payload(challenge: &[u8; 32]) -> [u8; 50] {
    let mut bytes = [0; 50];
    bytes[..32].copy_from_slice(challenge);
    bytes[32..].copy_from_slice(b"rusttalk-server-v1");
    bytes
}

pub fn auth_payload(challenge: &[u8; 32], server_id: &ServerId) -> [u8; 80] {
    let mut bytes = [0; 80];
    bytes[..32].copy_from_slice(challenge);
    bytes[32..64].copy_from_slice(server_id);
    bytes[64..].copy_from_slice(b"rusttalk-auth-v1");
    bytes
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownServer {
    /// The canonical HTTPS origin, with a trailing slash.
    pub address: Url,
    pub server_id: ServerId,
    pub tls_fingerprint: Option<Fingerprint>,
}

/// The client address from exactly one `X-Forwarded-For` value holding one IP address.
/// A missing, repeated or list-valued header yields `None`.
pub fn forwarded_ip<'a>(mut values: impl Iterator<Item = &'a [u8]>) -> Option<std::net::IpAddr> {
    let ip = std::str::from_utf8(values.next()?).ok()?.parse().ok()?;
    values.next().is_none().then_some(ip)
}

pub fn check_pin(known: Option<&KnownServer>, observed: &KnownServer) -> Result<(), ProtocolError> {
    let Some(known) = known else {
        return Ok(());
    };
    if known.address != observed.address || known.server_id != observed.server_id {
        return Err(ProtocolError::ServerIdChanged);
    }
    if known.tls_fingerprint != observed.tls_fingerprint {
        return Err(ProtocolError::TlsChanged);
    }
    Ok(())
}

pub struct InviteLink {
    pub server: KnownServer,
    pub code: InviteCode,
    pub link_key: Option<Key>,
}

impl InviteLink {
    pub fn parse(link: &str) -> Result<Self, ProtocolError> {
        let rest = link
            .strip_prefix("rusttalk://")
            .ok_or(ProtocolError::Invite)?;
        if rest.chars().any(|c| c.is_whitespace() || c == '\\') {
            return Err(ProtocolError::Invite);
        }
        let (location, pins) = rest.split_once('#').ok_or(ProtocolError::Invite)?;
        let (authority, code) = location.split_once("/i/").ok_or(ProtocolError::Invite)?;
        if authority.is_empty() || authority.contains(['/', '?', '@', '%']) {
            return Err(ProtocolError::Invite);
        }
        let address =
            Url::parse(&format!("https://{authority}/")).map_err(|_| ProtocolError::Invite)?;
        let (pins, link_key) = match pins.split_once(".k~") {
            Some((pins, key)) => (pins, Some(Key::new(decode(key)?))),
            None => (pins, None),
        };
        let (server_id, tls_fingerprint) = match pins.split_once('.') {
            Some((id, fp)) => (decode(id)?, Some(decode(fp)?)),
            None => (decode(pins)?, None),
        };
        Ok(Self {
            server: KnownServer {
                address,
                server_id,
                tls_fingerprint,
            },
            code: decode(code)?,
            link_key,
        })
    }
}

impl std::fmt::Display for InviteLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let authority = &self.server.address[url::Position::BeforeHost..url::Position::AfterPort];
        write!(
            f,
            "rusttalk://{authority}/i/{}#{}",
            encode(&self.code),
            encode(&self.server.server_id)
        )?;
        if let Some(fp) = self.server.tls_fingerprint {
            write!(f, ".{}", encode(&fp))?;
        }
        if let Some(key) = &self.link_key {
            write!(f, ".k~{}", encode(key.as_ref()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_without_max_files_take_four() {
        let info = |json| {
            serde_json::from_str::<ServerInfo>(json)
                .unwrap()
                .max_files()
        };
        assert_eq!(info(r#"{"open_joining":true}"#), 4);
        assert_eq!(info(r#"{"open_joining":true,"max_files":10}"#), 10);
        // A server that says more than this app can send still gets at most ten.
        assert_eq!(info(r#"{"open_joining":true,"max_files":200}"#), 10);
    }

    #[test]
    fn server_proof_binds_the_challenge_to_the_expected_key() {
        let key = SigningKey::from_bytes(&[4; 32]);
        let id = key.verifying_key().to_bytes();
        let mut proof = Challenge::signed(&[3; 32], &key);
        let signature = Signature::from_bytes(&decode(&proof.signature).unwrap());
        let expected = [[3; 32].as_slice(), b"rusttalk-server-v1"].concat();
        key.verifying_key()
            .verify_strict(&expected, &signature)
            .unwrap();
        assert_eq!(proof.verify(&id), Ok([3; 32]));
        assert_eq!(proof.verify(&[0; 32]), Err(ProtocolError::ServerIdChanged));
        proof.challenge = encode(&[2; 32]);
        assert_eq!(proof.verify(&id), Err(ProtocolError::ServerProof));

        // A flipped signature bit, or a valid signature by another key naming this server.
        let mut tampered = Challenge::signed(&[3; 32], &key);
        let mut bytes = decode::<64>(&tampered.signature).unwrap();
        bytes[0] ^= 1;
        tampered.signature = encode(&bytes);
        assert_eq!(tampered.verify(&id), Err(ProtocolError::ServerProof));
        let mut forged = Challenge::signed(&[3; 32], &SigningKey::from_bytes(&[5; 32]));
        forged.server_id = encode(&id);
        assert_eq!(forged.verify(&id), Err(ProtocolError::ServerProof));
    }

    #[test]
    fn first_use_and_same_pin_succeed_but_changes_are_refused() {
        let original = KnownServer {
            address: Url::parse("https://127.0.0.1:8443").unwrap(),
            server_id: [1; 32],
            tls_fingerprint: Some([2; 32]),
        };
        assert!(check_pin(None, &original).is_ok());
        assert!(check_pin(Some(&original), &original).is_ok());
        let mut changed = original.clone();
        changed.server_id = [9; 32];
        assert_eq!(
            check_pin(Some(&original), &changed),
            Err(ProtocolError::ServerIdChanged)
        );
        changed.server_id = original.server_id;
        for changed_fp in [Some([0; 32]), None] {
            changed.tls_fingerprint = changed_fp;
            assert_eq!(
                check_pin(Some(&original), &changed),
                Err(ProtocolError::TlsChanged)
            );
        }
    }

    #[test]
    fn invite_links_round_trip_and_reject_bad_fields() {
        for tls_fingerprint in [None, Some([23; 32])] {
            let link = InviteLink {
                server: KnownServer {
                    address: Url::parse("https://[::1]:8443").unwrap(),
                    server_id: [5; 32],
                    tls_fingerprint,
                },
                code: [7; 16],
                link_key: Some(Key::new([8; 32])),
            };
            let encoded = link.to_string();
            let decoded = InviteLink::parse(&encoded).unwrap();
            assert!(
                decoded.server == link.server
                    && decoded.code == link.code
                    && decoded.link_key == link.link_key
            );
            for bad in [
                encoded.replace("/i/", "/x/"),
                encoded.replace("/i/", "/i/%"),
                encoded.replace("/i/", "/i/short?"),
                encoded.replace("[::1]", "user@[::1]"),
                format!("{encoded}."),
                format!(" {encoded}"),
                encoded.replace("/i/", "//i/"),
            ] {
                assert!(InviteLink::parse(&bad).is_err());
            }
        }
        assert!(decode::<16>("AAAAAAAAAAAAAAAAAAAAAB").is_err());
        assert!(decode::<16>("AAAAAAAAAAAAAAAAAAAAAA==").is_err());
    }
}
