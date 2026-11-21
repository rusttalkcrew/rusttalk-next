//! W07 signed ciphertext and message records.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::keys::wire_id;

const DOMAIN: &[u8] = b"rusttalk-message-v1";
/// Envelope and plaintext format version. Version 2 carries stored and live attachments.
const VERSION: u8 = 2;
const NONCE: usize = 24;
const TAG: usize = 16;
const SIGNATURE: usize = 64;
const HEADER: usize = 1 + NONCE + 2;
/// Flags, text length and attachment count.
const MIN_PLAINTEXT: usize = 1 + 2 + 1;
const MAX_TEXT_BYTES: usize = MAX_TEXT_CHARS * 4;
/// Every field at its maximum: text, both references, four stored files and a live offer.
const MAX_PLAINTEXT: usize = MIN_PLAINTEXT
    + MAX_TEXT_BYTES
    + 2 * 32
    + MAX_ATTACHMENTS * MAX_ATTACHMENT_BYTES
    + live::MAX_ATTACHMENT;

/// Stored files per message.
const MAX_ATTACHMENTS: usize = 4;
const MAX_ATTACHMENT_BYTES: usize = 529;
/// File name length in UTF-8 bytes, for stored and live files.
const MAX_FILE_NAME: usize = 255;
/// Media type length in ASCII bytes, for stored and live files.
const MAX_MEDIA_TYPE: usize = 127;

const _: () = assert!(MAX_PLAINTEXT == 10_644);
const _: () = assert!(MAX_MESSAGE_BYTES == 10_751);

mod live;
#[cfg(any(feature = "client", test))]
pub use live::LiveAttachment;
pub use live::{LIVE_CHUNK_SIZE, LiveFileAcceptance, MAX_LIVE_FILE_SIZE};

/// Maximum message length in Unicode scalar values.
pub const MAX_TEXT_CHARS: usize = 2_000;
/// Maximum length of a poke's note in Unicode scalar values.
pub const MAX_POKE_CHARS: usize = 100;
/// Maximum signed binary envelope length.
pub const MAX_MESSAGE_BYTES: usize = HEADER + MAX_PLAINTEXT + TAG + SIGNATURE;

/// Whether a file name is safe to show: no paths, control or bidirectional formatting characters.
pub fn valid_file_name(name: &str) -> bool {
    (1..=MAX_FILE_NAME).contains(&name.len())
        && !matches!(name, "." | "..")
        && !name.chars().any(unsafe_name_char)
}

/// Make a local file name valid for a message: replace unsafe characters and shorten it,
/// keeping a short extension.
pub fn fit_file_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| if unsafe_name_char(c) { '_' } else { c })
        .collect();
    if name.is_empty() || matches!(name.as_str(), "." | "..") {
        return "file".into();
    }
    if name.len() <= MAX_FILE_NAME {
        return name;
    }
    let extension = name
        .rfind('.')
        .map(|at| &name[at..])
        .filter(|extension| extension.len() <= 16)
        .unwrap_or("");
    let stem = &name[..name.len() - extension.len()];
    let stem = &stem[..stem.floor_char_boundary(MAX_FILE_NAME - extension.len())];
    format!("{stem}{extension}")
}

/// Bidirectional formatting characters can disguise a name's extension when displayed.
fn unsafe_name_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '/' | '\\' | ':')
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

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
pub use crypto::{FriendSignal, KeyLink, MessageBody};

#[cfg(any(feature = "client", test))]
mod attachment;
#[cfg(any(feature = "client", test))]
pub use attachment::{Attachment, BlobRef};
#[cfg(feature = "client")]
pub(crate) use attachment::{CHUNK_BYTES, MAX_CIPHERTEXT, MAX_THUMBNAIL_BYTES, TAG_BYTES};
