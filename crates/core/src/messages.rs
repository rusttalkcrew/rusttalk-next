//! W07 signed ciphertext and message records.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::keys::wire_id;

const DOMAIN: &[u8] = b"rusttalk-message-v1";
const SHARED_FILE_DOMAIN: &[u8] = b"rusttalk-shared-file-v1";
/// A shared file's envelope holds one file reference, so it stays well under this.
pub const MAX_SHARED_ENVELOPE: usize = 1024;
const VERSION: u8 = 3;
const NONCE: usize = 24;
const TAG: usize = 16;
const SIGNATURE: usize = 64;
const HEADER: usize = 1 + NONCE + 2;
/// Flags, text length and attachment count: format 2's smallest message.
const MIN_PLAINTEXT: usize = 1 + 2 + 1;
/// Format 3's kind byte after the flags.
const KIND: usize = 1;
const MAX_TEXT_BYTES: usize = MAX_TEXT_CHARS * 4;
/// The kind byte, maximum text, two references (reply ID and quoted author, or
/// revision and edit ID), ten stored files and a live offer.
const MAX_PLAINTEXT: usize = MIN_PLAINTEXT
    + KIND
    + MAX_TEXT_BYTES
    + 2 * 32
    + MAX_ATTACHMENTS * MAX_ATTACHMENT_BYTES
    + live::MAX_ATTACHMENT;

/// Stored files per message.
pub const MAX_ATTACHMENTS: usize = 10;
/// Stored files per message on servers that don't say how many they take (0.8.1 and older),
/// whose message limit fits only four.
pub const LEGACY_MAX_ATTACHMENTS: usize = 4;
const MAX_ATTACHMENT_BYTES: usize = 529;
/// File name length in UTF-8 bytes, for stored and live files.
const MAX_FILE_NAME: usize = 255;
/// Media type length in ASCII bytes, for stored and live files.
const MAX_MEDIA_TYPE: usize = 127;

const _: () = assert!(MAX_PLAINTEXT == 13_819);
const _: () = assert!(MAX_MESSAGE_BYTES == 13_926);

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
    #[error("server update required for device removal rotation")]
    ServerUpdateRequired,
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

/// What a signed envelope is for. Each has its own associated-data label, so a chat
/// message can't pass as a shared file's record, or the other way round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Message,
    /// A file kept in a channel's file list, under the file's ID.
    SharedFile,
}

impl MessageContext {
    fn associated_data(self, purpose: Purpose) -> Vec<u8> {
        let domain = match purpose {
            Purpose::Message => DOMAIN,
            Purpose::SharedFile => SHARED_FILE_DOMAIN,
        };
        [
            domain,
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
        self.verify_as(Purpose::Message, sender, envelope)
    }

    /// `verify` for an envelope made for `purpose`.
    pub fn verify_as(
        self,
        purpose: Purpose,
        sender: &VerifyingKey,
        envelope: &[u8],
    ) -> Result<(), MessageError> {
        if self.epoch <= 0
            || !(HEADER + TAG + MIN_PLAINTEXT + SIGNATURE..=MAX_MESSAGE_BYTES)
                .contains(&envelope.len())
            || !matches!(envelope[0], 2 | VERSION)
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
                &[self.associated_data(purpose).as_slice(), sealed].concat(),
                &Signature::from_bytes(signature),
            )
            .map_err(|_| MessageError::Signature)
    }
}

/// Signed ciphertext and its server-provided routing and paging fields.
#[derive(Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    #[serde(default)]
    pub backfilled: bool,
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
mod poll;
#[cfg(any(feature = "client", test))]
pub use poll::{
    Ballot, MAX_POLL_OPTION, MAX_POLL_OPTIONS, MAX_POLL_QUESTION, MIN_POLL_OPTIONS, Poll, PollPart,
    Tally, tally, valid_poll,
};
#[cfg(any(feature = "client", test))]
mod reaction;
#[cfg(any(feature = "client", test))]
pub use reaction::{MAX_REACTION_EMOJI, Reaction, ReactionEmoji, reaction_vote, tally_reactions};
#[cfg(any(feature = "client", test))]
mod voice_request;
#[cfg(any(feature = "client", test))]
pub use voice_request::{VoiceAnswer, VoiceRequest};

// A poll is sent alone, so its kind, fields and question fit the plaintext limit.
#[cfg(any(feature = "client", test))]
const _: () = assert!(MIN_PLAINTEXT + MAX_POLL_QUESTION * 4 + poll::MAX_BYTES <= MAX_PLAINTEXT);

#[cfg(any(feature = "client", test))]
mod attachment;
#[cfg(any(feature = "client", test))]
pub(crate) use attachment::CHUNK_BYTES;
#[cfg(any(feature = "client", test))]
pub use attachment::{Attachment, BlobRef};
#[cfg(feature = "client")]
pub(crate) use attachment::{MAX_CIPHERTEXT, MAX_THUMBNAIL_BYTES, TAG_BYTES};
