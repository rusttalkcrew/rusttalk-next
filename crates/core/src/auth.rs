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
    pub person_id: String,
    pub certificate: String,
    pub challenge: String,
    pub signature: String,
    pub invite: Option<String>,
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
