use std::time::{SystemTime, SystemTimeError, UNIX_EPOCH};

use bip39::{Language, Mnemonic};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use rand::{
    SeedableRng, TryRng,
    rngs::{StdRng, SysRng},
    seq::index,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use x25519_dalek::{PublicKey as AgreementPublicKey, StaticSecret};
use zeroize::Zeroizing;

const WORD_COUNT: usize = 24;
const MAX_DEVICE_NAME_BYTES: usize = 64;
const CERT_DOMAIN: &[u8] = b"rusttalk-device-cert-v1";
const REVOCATION_DOMAIN: &[u8] = b"rusttalk-device-revocation-v1";

pub type PersonId = [u8; 32];

#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("expected 24 valid English BIP-39 words")]
    InvalidWords,
    #[error("could not read randomness from the operating system")]
    Randomness,
    #[error("system clock is before the Unix epoch")]
    Clock(#[from] SystemTimeError),
    #[error("device name must be 1 to 64 UTF-8 bytes")]
    InvalidDeviceName,
    #[error("invalid identity record encoding")]
    InvalidEncoding,
    #[error("invalid Ed25519 public key")]
    InvalidPublicKey,
    #[error("invalid root signature")]
    InvalidSignature,
    #[error("device keys do not match the certificate")]
    KeyMismatch,
}

pub struct DeviceIdentity {
    pub signing_key: SigningKey,
    pub agreement_key: StaticSecret,
    pub sync_key: Zeroizing<[u8; 32]>,
    pub certificate: DeviceCertificate,
}

impl DeviceIdentity {
    pub fn create(name: &str) -> Result<(Zeroizing<String>, Self), IdentityError> {
        let mut entropy = Zeroizing::new([0; 32]);
        random_bytes(entropy.as_mut())?;
        let mnemonic = Mnemonic::from_entropy_in(Language::English, entropy.as_ref())
            .expect("32 bytes is valid BIP-39 entropy");
        let identity = Self::from_mnemonic(&mnemonic, name)?;
        Ok((Zeroizing::new(mnemonic.to_string()), identity))
    }

    /// Restore a person on a fresh device with a new key pair and certificate.
    pub fn add_device(words: &str, name: &str) -> Result<Self, IdentityError> {
        Self::from_mnemonic(&parse_words(words)?, name)
    }

    fn from_mnemonic(mnemonic: &Mnemonic, name: &str) -> Result<Self, IdentityError> {
        if !(1..=MAX_DEVICE_NAME_BYTES).contains(&name.len()) {
            return Err(IdentityError::InvalidDeviceName);
        }
        let (root, sync_key) = derive_keys(mnemonic);
        let mut signing_bytes = Zeroizing::new([0; 32]);
        let mut agreement_bytes = Zeroizing::new([0; 32]);
        random_bytes(signing_bytes.as_mut())?;
        random_bytes(agreement_bytes.as_mut())?;
        let signing_key = SigningKey::from_bytes(&signing_bytes);
        let agreement_key = StaticSecret::from(*agreement_bytes);

        let device_signing_key = signing_key.verifying_key();
        let device_agreement_key = AgreementPublicKey::from(&agreement_key).to_bytes();
        let created_at = unix_seconds()?;
        let payload =
            certificate_payload(&device_signing_key, &device_agreement_key, created_at, name);
        let certificate = DeviceCertificate {
            person_id: root.verifying_key().to_bytes(),
            device_signing_key,
            device_agreement_key,
            created_at,
            name: name.to_owned(),
            signature: root.sign(&payload),
        };
        Ok(Self {
            signing_key,
            agreement_key,
            sync_key,
            certificate,
        })
    }

    /// Reconstruct a stored identity and check both device keys against its certificate.
    pub fn from_key_bytes(
        signing_key: &[u8; 32],
        agreement_key: &[u8; 32],
        sync_key: &[u8; 32],
        certificate: DeviceCertificate,
    ) -> Result<Self, IdentityError> {
        let signing_key = SigningKey::from_bytes(signing_key);
        let agreement_key = StaticSecret::from(*agreement_key);
        if signing_key.verifying_key() != certificate.device_signing_key
            || AgreementPublicKey::from(&agreement_key).as_bytes()
                != &certificate.device_agreement_key
        {
            return Err(IdentityError::KeyMismatch);
        }
        Ok(Self {
            signing_key,
            agreement_key,
            sync_key: Zeroizing::new(*sync_key),
            certificate,
        })
    }
}

#[derive(Debug)]
pub struct DeviceCertificate {
    person_id: PersonId,
    device_signing_key: VerifyingKey,
    device_agreement_key: [u8; 32],
    created_at: u64,
    name: String,
    signature: Signature,
}

impl DeviceCertificate {
    pub fn person_id(&self) -> &PersonId {
        &self.person_id
    }

    pub fn signing_key(&self) -> &VerifyingKey {
        &self.device_signing_key
    }

    pub fn agreement_key(&self) -> &[u8; 32] {
        &self.device_agreement_key
    }

    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = certificate_payload(
            &self.device_signing_key,
            &self.device_agreement_key,
            self.created_at,
            &self.name,
        );
        bytes.extend_from_slice(&self.signature.to_bytes());
        bytes
    }

    pub fn verify(person_id: PersonId, bytes: &[u8]) -> Result<Self, IdentityError> {
        let (payload, signature) = bytes
            .split_last_chunk()
            .ok_or(IdentityError::InvalidEncoding)?;
        let fields = payload
            .strip_prefix(CERT_DOMAIN)
            .ok_or(IdentityError::InvalidEncoding)?;
        let (signing, fields) = fields
            .split_first_chunk()
            .ok_or(IdentityError::InvalidEncoding)?;
        let (agreement, fields) = fields
            .split_first_chunk()
            .ok_or(IdentityError::InvalidEncoding)?;
        let (created_at, fields) = fields
            .split_first_chunk()
            .ok_or(IdentityError::InvalidEncoding)?;
        let Some((&name_len, name)) = fields.split_first() else {
            return Err(IdentityError::InvalidEncoding);
        };
        if name.len() != usize::from(name_len) || !(1..=MAX_DEVICE_NAME_BYTES).contains(&name.len())
        {
            return Err(IdentityError::InvalidEncoding);
        }
        let name = std::str::from_utf8(name).map_err(|_| IdentityError::InvalidEncoding)?;
        let signature = Signature::from_bytes(signature);
        verify_signature(&person_id, payload, &signature)?;

        Ok(Self {
            person_id,
            device_signing_key: public_key(signing)?,
            device_agreement_key: *agreement,
            created_at: u64::from_be_bytes(*created_at),
            name: name.to_owned(),
            signature,
        })
    }

    pub fn is_revoked(&self, revocations: &[Revocation]) -> bool {
        revocations.iter().any(|revocation| {
            revocation.person_id == self.person_id
                && revocation.device_signing_key == self.device_signing_key.to_bytes()
        })
    }
}

#[derive(Debug)]
pub struct Revocation {
    person_id: PersonId,
    device_signing_key: [u8; 32],
    revoked_at: u64,
    signature: Signature,
}

impl Revocation {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = revocation_payload(&self.device_signing_key, self.revoked_at);
        bytes.extend_from_slice(&self.signature.to_bytes());
        bytes
    }

    pub fn verify(person_id: PersonId, bytes: &[u8]) -> Result<Self, IdentityError> {
        let (payload, signature) = bytes
            .split_last_chunk()
            .ok_or(IdentityError::InvalidEncoding)?;
        let fields = payload
            .strip_prefix(REVOCATION_DOMAIN)
            .ok_or(IdentityError::InvalidEncoding)?;
        let (signing, time) = fields
            .split_first_chunk()
            .ok_or(IdentityError::InvalidEncoding)?;
        let time = time
            .try_into()
            .map_err(|_| IdentityError::InvalidEncoding)?;
        let signature = Signature::from_bytes(signature);
        verify_signature(&person_id, payload, &signature)?;
        public_key(signing)?;
        Ok(Self {
            person_id,
            device_signing_key: *signing,
            revoked_at: u64::from_be_bytes(time),
            signature,
        })
    }
}

pub fn revoke_device(
    words: &str,
    device_signing_key: [u8; 32],
) -> Result<Revocation, IdentityError> {
    public_key(&device_signing_key)?;
    let (root, _) = derive_keys(&parse_words(words)?);
    let revoked_at = unix_seconds()?;
    Ok(Revocation {
        person_id: root.verifying_key().to_bytes(),
        device_signing_key,
        revoked_at,
        signature: root.sign(&revocation_payload(&device_signing_key, revoked_at)),
    })
}

pub fn fingerprint(person_id: &PersonId) -> String {
    let digest = Sha256::digest(person_id);
    let [a, b, c, d, e, f, g, h] = *digest.first_chunk().expect("SHA-256 is 32 bytes");
    format!("{a:02x}{b:02x}-{c:02x}{d:02x}-{e:02x}{f:02x}-{g:02x}{h:02x}")
}

pub struct WordConfirmation([usize; 3]);

impl WordConfirmation {
    pub fn random() -> Result<Self, IdentityError> {
        let mut rng = StdRng::try_from_rng(&mut SysRng).map_err(|_| IdentityError::Randomness)?;
        let mut indices = [0; 3];
        for (slot, word_index) in indices
            .iter_mut()
            .zip(index::sample(&mut rng, WORD_COUNT, 3))
        {
            *slot = word_index;
        }
        indices.sort_unstable();
        Ok(Self(indices))
    }

    /// Zero-based positions in the recovery phrase, in ascending order.
    pub fn indices(&self) -> [usize; 3] {
        self.0
    }

    pub fn matches(&self, words: &str, answers: [&str; 3]) -> bool {
        let words = words.split_whitespace();
        words.clone().count() == WORD_COUNT
            && self.0.into_iter().zip(answers).all(|(index, answer)| {
                words
                    .clone()
                    .nth(index)
                    .is_some_and(|word| word.eq_ignore_ascii_case(answer.trim()))
            })
    }
}

fn parse_words(words: &str) -> Result<Mnemonic, IdentityError> {
    let mnemonic = Mnemonic::parse_in_normalized(Language::English, &words.to_lowercase())
        .map_err(|_| IdentityError::InvalidWords)?;
    if mnemonic.word_count() != WORD_COUNT {
        return Err(IdentityError::InvalidWords);
    }
    Ok(mnemonic)
}

fn derive_keys(mnemonic: &Mnemonic) -> (SigningKey, Zeroizing<[u8; 32]>) {
    let seed = Zeroizing::new(mnemonic.to_seed_normalized(""));
    let hkdf = Hkdf::<Sha256>::new(None, seed.as_ref());
    let mut root = Zeroizing::new([0; 32]);
    let mut sync = Zeroizing::new([0; 32]);
    hkdf.expand(b"rusttalk root v1", root.as_mut())
        .expect("32-byte HKDF output");
    hkdf.expand(b"rusttalk sync v1", sync.as_mut())
        .expect("32-byte HKDF output");
    (SigningKey::from_bytes(&root), sync)
}

fn random_bytes(bytes: &mut [u8]) -> Result<(), IdentityError> {
    SysRng
        .try_fill_bytes(bytes)
        .map_err(|_| IdentityError::Randomness)
}

fn unix_seconds() -> Result<u64, SystemTimeError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn certificate_payload(
    signing: &VerifyingKey,
    agreement: &[u8; 32],
    created_at: u64,
    name: &str,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(CERT_DOMAIN.len() + 32 + 32 + 8 + 1 + name.len() + 64);
    bytes.extend_from_slice(CERT_DOMAIN);
    bytes.extend_from_slice(signing.as_bytes());
    bytes.extend_from_slice(agreement);
    bytes.extend_from_slice(&created_at.to_be_bytes());
    bytes.push(name.len() as u8);
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

fn revocation_payload(signing: &[u8; 32], revoked_at: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(REVOCATION_DOMAIN.len() + 32 + 8 + 64);
    bytes.extend_from_slice(REVOCATION_DOMAIN);
    bytes.extend_from_slice(signing);
    bytes.extend_from_slice(&revoked_at.to_be_bytes());
    bytes
}

fn public_key(bytes: &[u8; 32]) -> Result<VerifyingKey, IdentityError> {
    let key = VerifyingKey::from_bytes(bytes).map_err(|_| IdentityError::InvalidPublicKey)?;
    if key.is_weak() {
        return Err(IdentityError::InvalidPublicKey);
    }
    Ok(key)
}

fn verify_signature(
    person_id: &PersonId,
    payload: &[u8],
    signature: &Signature,
) -> Result<(), IdentityError> {
    public_key(person_id)?
        .verify_strict(payload, signature)
        .map_err(|_| IdentityError::InvalidSignature)
}

#[cfg(test)]
mod tests;
