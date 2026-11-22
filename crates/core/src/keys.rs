//! W06 key envelopes and encrypted metadata formats.

use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use ed25519_dalek::{Signature, Signer};
use hkdf::hmac::{Hmac, Mac};
use hpke::{
    Deserializable, Kem, OpModeR, OpModeS, Serializable, aead::ChaCha20Poly1305, kdf::HkdfSha256,
    kem::X25519HkdfSha256, single_shot_open, single_shot_seal_with_rng,
};
use rand::{SeedableRng, rngs::StdRng};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{
    auth::{decode, encode},
    identity::{DeviceCertificate, DeviceIdentity, IdentityError, random_bytes},
};

type Dh = X25519HkdfSha256;
pub type Key = Zeroizing<[u8; 32]>;
pub const SMALL_METADATA: usize = 1024;
pub const IMAGE_CIPHERTEXT: usize = 256 * 1024;
const NONCE: usize = 24;
const TAG: usize = 16;
const HEADER: usize = 8 + NONCE;
/// The largest avatar, server icon or server emoji that still seals within `IMAGE_CIPHERTEXT`.
pub const IMAGE_PLAINTEXT: usize = IMAGE_CIPHERTEXT - HEADER - TAG;
pub const INVITE_BYTES: usize = NONCE + 8 + 32 + TAG;

#[derive(Debug, Error)]
pub enum KeyError {
    #[error("invalid key record")]
    Encoding,
    #[error("invalid envelope signature")]
    Signature,
    #[error("key encryption or authentication failed")]
    Authentication,
    #[error("key does not match the epoch commitment")]
    Commitment,
    #[error(transparent)]
    Randomness(#[from] IdentityError),
}

pub fn random_key() -> Result<Key, KeyError> {
    let mut key = Key::new([0; 32]);
    random_bytes(key.as_mut())?;
    Ok(key)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Space = 1,
    Channel = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub kind: Kind,
    #[serde(with = "wire_id")]
    pub id: [u8; 32],
}

pub(crate) mod wire_id {
    use super::*;

    pub fn serialize<S: serde::Serializer>(id: &[u8; 32], out: S) -> Result<S::Ok, S::Error> {
        out.serialize_str(&encode(id))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(input: D) -> Result<[u8; 32], D::Error> {
        decode(&String::deserialize(input)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeyNeed {
    pub scope: Scope,
    pub epoch: i64,
    pub device: String,
}

#[derive(Serialize, Deserialize)]
pub struct WrappedKey {
    #[serde(flatten)]
    pub need: KeyNeed,
    pub sender: String,
    #[serde(with = "wire_id")]
    pub commitment: [u8; 32],
    pub envelope: String,
}

#[derive(Serialize, Deserialize)]
pub struct ScopeState {
    pub scope: Scope,
    pub epoch: i64,
    pub rotate: bool,
}

#[derive(Serialize, Deserialize)]
pub struct KeyState {
    pub scopes: Vec<ScopeState>,
    pub envelopes: Vec<WrappedKey>,
    pub pending: Vec<KeyNeed>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    KeysNeeded(KeyNeed),
    RotateNeeded {
        scope: Scope,
    },
    MemberChanged {
        person: String,
        device: Option<String>,
    },
    ChannelChanged,
    VoiceChanged {
        channel: String,
    },
    /// A moderator moved these devices from one voice channel to another. Sent only to them.
    VoiceMoved {
        devices: Vec<String>,
        from: String,
        to: String,
    },
    DmCreated {
        channel: String,
    },
    MessageCreated {
        channel: String,
        id: String,
    },
    MessageDeleted {
        channel: String,
        id: String,
    },
    LiveFileAccepted {
        channel: String,
        device: String,
        acceptance: crate::messages::LiveFileAcceptance,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        data: bool,
    },
    Resync,
}

#[derive(Serialize, Deserialize)]
pub struct MemberDevice {
    pub person: String,
    pub certificate: String,
}

#[derive(Clone, Copy)]
pub struct EnvelopeContext {
    pub server_id: [u8; 32],
    pub scope: Scope,
    pub epoch: i64,
    pub commitment: [u8; 32],
}

impl EnvelopeContext {
    pub fn new(server_id: [u8; 32], scope: Scope, epoch: i64, key: &Key) -> Self {
        let mut context = Self {
            server_id,
            scope,
            epoch,
            commitment: [0; 32],
        };
        context.commitment = context.key_mac(key).finalize().into_bytes().into();
        context
    }

    /// For a DM epoch: the commitment also names the two people (`dm_pair`), so a
    /// key made for one pair can't pass as another's.
    pub fn new_dm(
        server_id: [u8; 32],
        scope: Scope,
        epoch: i64,
        key: &Key,
        pair: [[u8; 32]; 2],
    ) -> Self {
        let mut context = Self::new(server_id, scope, epoch, key);
        context.commitment = context.dm_mac(key, pair).finalize().into_bytes().into();
        context
    }

    fn key_mac(&self, key: &Key) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_ref())
            .expect("HMAC accepts any key length");
        mac.update(b"rusttalk-key-commit-v1");
        mac.update(&self.server_id);
        mac.update(&[self.scope.kind as u8]);
        mac.update(&self.scope.id);
        mac.update(&self.epoch.to_be_bytes());
        mac
    }

    fn dm_mac(&self, key: &Key, pair: [[u8; 32]; 2]) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key.as_ref())
            .expect("HMAC accepts any key length");
        mac.update(b"rusttalk-dm-key-commit-v1");
        mac.update(&self.server_id);
        mac.update(&[self.scope.kind as u8]);
        mac.update(&self.scope.id);
        mac.update(&self.epoch.to_be_bytes());
        mac.update(&pair[0]);
        mac.update(&pair[1]);
        mac
    }

    /// Whether this envelope's commitment is the DM commitment of `key` for `pair`.
    pub fn commits_dm(&self, key: &Key, pair: [[u8; 32]; 2]) -> bool {
        self.dm_mac(key, pair)
            .verify_slice(&self.commitment)
            .is_ok()
    }

    fn info(&self, recipient: &[u8; 32]) -> Vec<u8> {
        [
            b"rusttalk-envelope-v1".as_slice(),
            &self.server_id,
            &[self.scope.kind as u8],
            &self.scope.id,
            &self.epoch.to_be_bytes(),
            recipient,
        ]
        .concat()
    }

    pub fn wrap(
        &self,
        key: &Key,
        recipient: &DeviceCertificate,
        sender: &DeviceIdentity,
    ) -> Result<Vec<u8>, KeyError> {
        if self.epoch <= 0 {
            return Err(KeyError::Encoding);
        }
        let info = self.info(recipient.signing_key().as_bytes());
        let public = <Dh as Kem>::PublicKey::from_bytes(recipient.agreement_key())
            .map_err(|_| KeyError::Encoding)?;
        let mut rng = StdRng::from_seed(*random_key()?);
        let (enc, ciphertext) = single_shot_seal_with_rng::<ChaCha20Poly1305, HkdfSha256, Dh>(
            &OpModeS::Base,
            &public,
            &info,
            key.as_ref(),
            &[],
            &mut rng,
        )
        .map_err(|_| KeyError::Authentication)?;
        let mut bytes = [enc.to_bytes().as_slice(), ciphertext.as_slice()].concat();
        let signature = sender
            .signing_key
            .sign(&[info.as_slice(), &self.commitment, &bytes].concat());
        bytes.extend_from_slice(&signature.to_bytes());
        Ok(bytes)
    }

    pub fn verify(
        &self,
        recipient: &[u8; 32],
        sender: &DeviceCertificate,
        bytes: &[u8],
    ) -> Result<(), KeyError> {
        if self.epoch <= 0 || bytes.len() != 32 + 32 + TAG + 64 {
            return Err(KeyError::Encoding);
        }
        let (sealed, signature) = bytes.split_last_chunk().ok_or(KeyError::Encoding)?;
        let info = self.info(recipient);
        sender
            .signing_key()
            .verify_strict(
                &[info.as_slice(), &self.commitment, sealed].concat(),
                &Signature::from_bytes(signature),
            )
            .map_err(|_| KeyError::Signature)
    }

    pub fn unwrap(
        &self,
        recipient: &DeviceIdentity,
        sender: &DeviceCertificate,
        bytes: &[u8],
    ) -> Result<Key, KeyError> {
        let key = self.open(recipient, sender, bytes)?;
        self.key_mac(&key)
            .verify_slice(&self.commitment)
            .map_err(|_| KeyError::Commitment)?;
        Ok(key)
    }

    /// Verify and decrypt an envelope, leaving the commitment check to the caller
    /// (`unwrap` for the plain one, `commits_dm` for a DM's).
    pub fn open(
        &self,
        recipient: &DeviceIdentity,
        sender: &DeviceCertificate,
        bytes: &[u8],
    ) -> Result<Key, KeyError> {
        let device = recipient.certificate.signing_key().as_bytes();
        self.verify(device, sender, bytes)?;
        let info = self.info(device);
        let secret = <Dh as Kem>::PrivateKey::from_bytes(recipient.agreement_key.as_bytes())
            .map_err(|_| KeyError::Encoding)?;
        let enc =
            <Dh as Kem>::EncappedKey::from_bytes(&bytes[..32]).map_err(|_| KeyError::Encoding)?;
        let key = Zeroizing::new(
            single_shot_open::<ChaCha20Poly1305, HkdfSha256, Dh>(
                &OpModeR::Base,
                &secret,
                &enc,
                &info,
                &bytes[32..80],
                &[],
            )
            .map_err(|_| KeyError::Authentication)?,
        );
        Ok(Key::new(
            key.as_slice().try_into().map_err(|_| KeyError::Encoding)?,
        ))
    }

    /// Whether this envelope's commitment is the plain (non-DM) one for `key`.
    pub fn commits(&self, key: &Key) -> bool {
        self.key_mac(key).verify_slice(&self.commitment).is_ok()
    }
}

/// The two people of a DM in a fixed order, as DM commitments name them.
pub fn dm_pair(a: [u8; 32], b: [u8; 32]) -> [[u8; 32]; 2] {
    if a <= b { [a, b] } else { [b, a] }
}

#[derive(Clone, Copy)]
pub enum Metadata {
    ServerName = 1,
    ServerDescription,
    ServerIcon,
    ChannelName,
    ChannelTopic,
    CategoryName,
    RoleName,
    DisplayName,
    Avatar,
    EmojiName,
    EmojiImage,
    /// A role's colour and whether it is listed separately; see the client's role display.
    RoleDisplay,
    /// A member's profile bio: a few lines of plain text.
    Bio,
    EmojiPackName,
}

impl Metadata {
    fn limit(self) -> usize {
        match self {
            Self::ServerIcon | Self::Avatar | Self::EmojiImage => IMAGE_CIPHERTEXT,
            // W05 roles already have a 512-byte storage limit.
            Self::RoleName => 512,
            _ => SMALL_METADATA + HEADER + TAG,
        }
    }

    fn aad(self, server_id: &[u8; 32], id: &[u8], epoch: i64) -> Result<Vec<u8>, KeyError> {
        let len = u8::try_from(id.len()).map_err(|_| KeyError::Encoding)?;
        if len == 0 || epoch <= 0 {
            return Err(KeyError::Encoding);
        }
        Ok([
            b"rusttalk-meta-v1".as_slice(),
            server_id,
            &[self as u8, len],
            id,
            &epoch.to_be_bytes(),
        ]
        .concat())
    }

    pub fn epoch(self, bytes: &[u8]) -> Result<i64, KeyError> {
        if !(HEADER + TAG..=self.limit()).contains(&bytes.len()) {
            return Err(KeyError::Encoding);
        }
        let epoch = i64::from_be_bytes(*bytes.first_chunk().ok_or(KeyError::Encoding)?);
        if epoch <= 0 {
            return Err(KeyError::Encoding);
        }
        Ok(epoch)
    }

    pub fn seal(
        self,
        server_id: &[u8; 32],
        id: &[u8],
        epoch: i64,
        key: &Key,
        clear: &[u8],
    ) -> Result<Vec<u8>, KeyError> {
        if clear.len() > self.limit() - HEADER - TAG {
            return Err(KeyError::Encoding);
        }
        let ciphertext = seal(key, &self.aad(server_id, id, epoch)?, clear)?;
        Ok([epoch.to_be_bytes().as_slice(), &ciphertext].concat())
    }

    pub fn open(
        self,
        server_id: &[u8; 32],
        id: &[u8],
        key: &Key,
        bytes: &[u8],
    ) -> Result<Vec<u8>, KeyError> {
        let epoch = self.epoch(bytes)?;
        open(key, &self.aad(server_id, id, epoch)?, &bytes[8..])
    }
}

pub(crate) fn seal(key: &Key, aad: &[u8], msg: &[u8]) -> Result<Vec<u8>, KeyError> {
    let mut nonce = [0; NONCE];
    random_bytes(&mut nonce)?;
    let ct = XChaCha20Poly1305::new(&(**key).into())
        .encrypt(&nonce.into(), Payload { msg, aad })
        .map_err(|_| KeyError::Authentication)?;
    Ok([nonce.as_slice(), &ct].concat())
}

pub(crate) fn open(key: &Key, aad: &[u8], bytes: &[u8]) -> Result<Vec<u8>, KeyError> {
    let (nonce, msg) = bytes
        .split_first_chunk::<NONCE>()
        .ok_or(KeyError::Encoding)?;
    XChaCha20Poly1305::new(&(**key).into())
        .decrypt(&XNonce::from(*nonce), Payload { msg, aad })
        .map_err(|_| KeyError::Authentication)
}

pub fn seal_invite(
    server: &[u8; 32],
    epoch: i64,
    key: &Key,
    link: &Key,
) -> Result<Vec<u8>, KeyError> {
    if epoch <= 0 {
        return Err(KeyError::Encoding);
    }
    let mut clear = Zeroizing::new([0; 40]);
    clear[..8].copy_from_slice(&epoch.to_be_bytes());
    clear[8..].copy_from_slice(key.as_ref());
    seal(
        link,
        &[b"rusttalk-invite-v1".as_slice(), server].concat(),
        clear.as_ref(),
    )
}

pub fn open_invite(server: &[u8; 32], link: &Key, bytes: &[u8]) -> Result<(i64, Key), KeyError> {
    if bytes.len() != INVITE_BYTES {
        return Err(KeyError::Encoding);
    }
    let clear = Zeroizing::new(open(
        link,
        &[b"rusttalk-invite-v1".as_slice(), server].concat(),
        bytes,
    )?);
    let (epoch, key) = clear.split_first_chunk().ok_or(KeyError::Encoding)?;
    let epoch = i64::from_be_bytes(*epoch);
    if epoch <= 0 {
        return Err(KeyError::Encoding);
    }
    Ok((
        epoch,
        Key::new(key.try_into().map_err(|_| KeyError::Encoding)?),
    ))
}

#[cfg(test)]
mod tests;
