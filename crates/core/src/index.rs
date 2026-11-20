//! Public directory records and their versioned Ed25519 signature.

use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use url::Url;

use crate::{
    auth::{decode, encode},
    keys::{self, Key, KeyError},
};

pub const BODY_LIMIT: usize = 16 * 1024;
pub const CLOCK_SKEW_MS: i64 = 60_000;
const SIGNING_DOMAIN: &[u8] = b"rusttalk-index-v1";
const SETTINGS_DOMAIN: &[u8] = b"rusttalk-index-settings-v1";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Listing {
    pub server_id: String,
    pub name: String,
    pub description: String,
    pub address: String,
    pub member_count: u32,
    pub open_joining: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub listing: Listing,
    pub timestamp: i64,
    pub signature: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegistrationError {
    #[error("invalid listing fields")]
    Fields,
    #[error("registration timestamp is outside the freshness window")]
    Timestamp,
    #[error("invalid registration signature")]
    Signature,
}

impl Registration {
    pub fn signed(
        listing: Listing,
        timestamp: i64,
        key: &SigningKey,
    ) -> Result<Self, RegistrationError> {
        let mut record = Self {
            listing,
            timestamp,
            signature: String::new(),
        };
        let id = decode(&record.listing.server_id).map_err(|_| RegistrationError::Signature)?;
        record.signature = encode(&key.sign(&record.signing_bytes(&id)?).to_bytes());
        Ok(record)
    }

    pub fn verify(&self, now: i64) -> Result<(), RegistrationError> {
        if now.abs_diff(self.timestamp) > CLOCK_SKEW_MS as u64 {
            return Err(RegistrationError::Timestamp);
        }
        let id = decode(&self.listing.server_id).map_err(|_| RegistrationError::Signature)?;
        let bytes = self.signing_bytes(&id)?;
        let key = VerifyingKey::from_bytes(&id).map_err(|_| RegistrationError::Signature)?;
        let signature = decode(&self.signature).map_err(|_| RegistrationError::Signature)?;
        key.verify_strict(&bytes, &Signature::from_bytes(&signature))
            .map_err(|_| RegistrationError::Signature)
    }

    fn signing_bytes(&self, id: &[u8; 32]) -> Result<Vec<u8>, RegistrationError> {
        let Listing {
            name,
            description,
            address,
            member_count,
            open_joining,
            ..
        } = &self.listing;
        validate_details(name, description, address)?;
        let mut bytes = SIGNING_DOMAIN.to_vec();
        bytes.extend_from_slice(id);
        bytes.extend_from_slice(&self.timestamp.to_be_bytes());
        bytes.extend_from_slice(&member_count.to_be_bytes());
        bytes.push(u8::from(*open_joining));
        for text in [name, description, address] {
            bytes.extend_from_slice(&(text.len() as u16).to_be_bytes());
            bytes.extend_from_slice(text.as_bytes());
        }
        Ok(bytes)
    }
}

pub fn validate_details(
    name: &str,
    description: &str,
    address: &str,
) -> Result<(), RegistrationError> {
    if name.trim().is_empty()
        || name.chars().count() > 100
        || name.chars().any(|ch| ch.is_control() || hidden_format(ch))
        || description.chars().count() > 1_000
        || description
            .chars()
            .any(|ch| (ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')) || hidden_format(ch))
        || !https_url(address).is_some_and(|url| url.path() == "/")
        || address
            .strip_prefix("https://")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, path)| !path.is_empty())
    {
        return Err(RegistrationError::Fields);
    }
    Ok(())
}

/// Bidi overrides, embeddings and isolates can make a listing display as different text,
/// and invisible separators can make two names look identical.
fn hidden_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{200B}' | '\u{202A}'..='\u{202E}' | '\u{2060}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

pub fn https_url(raw: &str) -> Option<Url> {
    let authority = raw.strip_prefix("https://")?.split('/').next()?;
    if raw.len() > 512
        || raw.contains(['\\', '?', '#'])
        || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        || authority.is_empty()
        || authority.contains('@')
        || authority.ends_with(':')
    {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    (url.has_host() && url.port() != Some(0)).then_some(url)
}

pub fn now_ms() -> Option<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

fn settings_key(server_key: &SigningKey) -> Key {
    let mut key = Key::new([0; 32]);
    Hkdf::<Sha256>::new(None, server_key.as_bytes())
        .expand(SETTINGS_DOMAIN, key.as_mut())
        .expect("32-byte HKDF output");
    key
}

/// Protect the explicitly public copy in the API's SQLite file using the local server key.
pub fn seal_settings(server_key: &SigningKey, clear: &[u8]) -> Result<Vec<u8>, KeyError> {
    keys::seal(&settings_key(server_key), SETTINGS_DOMAIN, clear)
}

pub fn open_settings(server_key: &SigningKey, ciphertext: &[u8]) -> Result<Vec<u8>, KeyError> {
    keys::open(&settings_key(server_key), SETTINGS_DOMAIN, ciphertext)
}
