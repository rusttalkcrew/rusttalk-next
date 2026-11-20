//! W07 signed ciphertext and message records.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::keys::wire_id;

const DOMAIN: &[u8] = b"rusttalk-message-v1";
const VERSION: u8 = 1;
const NONCE: usize = 24;
const TAG: usize = 16;
const SIGNATURE: usize = 64;
const HEADER: usize = 1 + NONCE + 2;
const MIN_PLAINTEXT: usize = 4;
const MAX_PLAINTEXT: usize =
    MIN_PLAINTEXT + 2 * 32 + MAX_TEXT_CHARS * 4 + MAX_ATTACHMENTS * MAX_ATTACHMENT_BYTES;

const MAX_ATTACHMENTS: usize = 4;
const MAX_ATTACHMENT_BYTES: usize = 529;

/// Maximum message length in Unicode scalar values.
pub const MAX_TEXT_CHARS: usize = 2_000;
/// Maximum signed binary envelope length.
pub const MAX_MESSAGE_BYTES: usize = HEADER + MAX_PLAINTEXT + TAG + SIGNATURE;

/// A message that could not be authenticated or decoded.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MessageError {
    #[error("invalid message encoding")]
    Encoding,
    #[error("invalid message signature or sender")]
    Signature,
    #[error("message encryption or authentication failed")]
    Authentication,
    #[error("invalid attachment reference")]
    Attachment,
    #[error("message text exceeds 2,000 characters")]
    TextLimit,
    #[error("message randomness unavailable")]
    Randomness,
    #[error("missing message epoch key")]
    MissingKey,
    #[error("message ID was already received")]
    Replay,
    #[error("edit must refer to an original message by the same person")]
    EditAuthor,
}

/// Context authenticated by both encryption and the device signature.
#[derive(Clone, Copy)]
pub struct MessageContext {
    pub server_id: [u8; 32],
    pub channel_id: [u8; 32],
    pub epoch: i64,
    pub id: [u8; 32],
    pub sender_device: [u8; 32],
    pub person: [u8; 32],
}

impl MessageContext {
    fn associated_data(self) -> Vec<u8> {
        [
            DOMAIN,
            &self.server_id,
            &self.channel_id,
            &self.epoch.to_be_bytes(),
            &self.id,
            &self.sender_device,
            &self.person,
        ]
        .concat()
    }

    /// Verify the public envelope framing and its signature against the sender's device key.
    pub fn verify(self, sender: &VerifyingKey, envelope: &[u8]) -> Result<(), MessageError> {
        if self.epoch <= 0
            || !(HEADER + TAG + MIN_PLAINTEXT + SIGNATURE..=MAX_MESSAGE_BYTES)
                .contains(&envelope.len())
            || envelope[0] != VERSION
        {
            return Err(MessageError::Encoding);
        }
        let (sealed, signature) = envelope.split_last_chunk().ok_or(MessageError::Encoding)?;
        let length = u16::from_be_bytes([sealed[HEADER - 2], sealed[HEADER - 1]]) as usize;
        if sealed.len() != HEADER + length {
            return Err(MessageError::Encoding);
        }
        if sender.as_bytes() != &self.sender_device {
            return Err(MessageError::Signature);
        }
        sender
            .verify_strict(
                &[self.associated_data().as_slice(), sealed].concat(),
                &Signature::from_bytes(signature),
            )
            .map_err(|_| MessageError::Signature)
    }
}

/// Signed ciphertext and its server-provided routing and paging fields.
#[derive(Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    #[serde(with = "wire_id")]
    pub id: [u8; 32],
    pub epoch: i64,
    #[serde(with = "wire_id")]
    pub person: [u8; 32],
    #[serde(with = "wire_id")]
    pub sender_device: [u8; 32],
    pub at: i64,
    pub size: usize,
    pub envelope: String,
}

#[cfg(any(feature = "client", test))]
mod crypto;
#[cfg(any(feature = "client", test))]
pub use crypto::MessageBody;

#[cfg(any(feature = "client", test))]
mod attachment;
#[cfg(any(feature = "client", test))]
pub use attachment::{Attachment, BlobRef};
#[cfg(feature = "client")]
pub(crate) use attachment::{CHUNK_BYTES, MAX_CIPHERTEXT, MAX_THUMBNAIL_BYTES, TAG_BYTES};
