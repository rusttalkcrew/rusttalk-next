use crate::keys::Key;

use super::{MAX_FILE_NAME as MAX_NAME, MAX_MEDIA_TYPE as MAX_TYPE, MessageError, valid_file_name};

const BLOB_BYTES: usize = 32 + 32 + 4 + 4;
const _: () = assert!(super::MAX_ATTACHMENT_BYTES == 2 * BLOB_BYTES + 3 + MAX_NAME + MAX_TYPE);
pub(crate) const TAG_BYTES: usize = 16;
#[cfg(any(feature = "client", test))]
pub(crate) const CHUNK_BYTES: u32 = 64 * 1024;
pub(crate) const MAX_CIPHERTEXT: usize = 32 * 1024 * 1024;
const _: () = assert!(crate::auth::FileLimits::MAX.max_file_bytes as usize == MAX_CIPHERTEXT);
pub(crate) const MAX_THUMBNAIL_BYTES: usize = 512 * 1024;

type Result<T> = std::result::Result<T, MessageError>;

/// The private material needed to retrieve and authenticate one encrypted blob.
#[derive(Clone, PartialEq, Eq)]
pub struct BlobRef {
    pub id: [u8; 32],
    pub size: u32,
    pub chunk_size: u32,
    pub(crate) key: Key,
}

impl BlobRef {
    /// A reference that only carries an ID, for other crates' tests; it opens nothing.
    #[doc(hidden)]
    pub fn for_tests(id: [u8; 32]) -> Self {
        Self {
            id,
            size: 0,
            chunk_size: 64 * 1024,
            key: Key::new([0; 32]),
        }
    }

    pub(crate) fn ciphertext_len(&self) -> Result<usize> {
        if !(16 * 1024..=1024 * 1024).contains(&self.chunk_size)
            || !self.chunk_size.is_power_of_two()
        {
            return Err(MessageError::Attachment);
        }
        let size = self.size as usize;
        let chunks = size.div_ceil(self.chunk_size as usize).max(1);
        size.checked_add(chunks * TAG_BYTES)
            .filter(|bytes| *bytes <= MAX_CIPHERTEXT)
            .ok_or(MessageError::Attachment)
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.id);
        out.extend_from_slice(self.key.as_ref());
        out.extend_from_slice(&self.size.to_be_bytes());
        out.extend_from_slice(&self.chunk_size.to_be_bytes());
    }

    fn decode(input: &mut &[u8]) -> Result<Self> {
        let (id, rest) = input
            .split_first_chunk::<32>()
            .ok_or(MessageError::Attachment)?;
        let (key, rest) = rest
            .split_first_chunk::<32>()
            .ok_or(MessageError::Attachment)?;
        let (size, rest) = rest.split_first_chunk().ok_or(MessageError::Attachment)?;
        let (chunk_size, rest) = rest.split_first_chunk().ok_or(MessageError::Attachment)?;
        *input = rest;
        Ok(Self {
            id: *id,
            key: Key::new(*key),
            size: u32::from_be_bytes(*size),
            chunk_size: u32::from_be_bytes(*chunk_size),
        })
    }
}

/// A server-stored attachment carried only inside a verified, decrypted message.
#[derive(Clone, PartialEq, Eq)]
pub struct Attachment {
    pub file: BlobRef,
    pub name: String,
    pub media_type: String,
    pub thumbnail: Option<BlobRef>,
}

/// A media type parameter marking a picture or video its sender called sensitive.
/// It rides in the encrypted media type, so the message format and its size limit stay
/// as they are, and older apps still read the type before the `;`.
pub const SENSITIVE: &str = ";sensitive";

impl Attachment {
    /// Whether the sender marked this file as sensitive.
    pub fn sensitive(&self) -> bool {
        self.media_type.ends_with(SENSITIVE)
    }

    /// The media type without the sensitive mark.
    pub fn plain_media_type(&self) -> &str {
        self.media_type
            .strip_suffix(SENSITIVE)
            .unwrap_or(&self.media_type)
    }

    /// Mark the file as sensitive; `false` when its media type has no room left.
    pub fn mark_sensitive(&mut self) -> bool {
        if self.sensitive() {
            return true;
        }
        if self.media_type.len() + SENSITIVE.len() > MAX_TYPE {
            return false;
        }
        self.media_type.push_str(SENSITIVE);
        true
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.file.ciphertext_len()?;
        if !valid_file_name(&self.name)
            || !(1..=MAX_TYPE).contains(&self.media_type.len())
            || !self.media_type.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(MessageError::Attachment);
        }
        if let Some(thumb) = &self.thumbnail {
            thumb.ciphertext_len()?;
            if thumb.size as usize > MAX_THUMBNAIL_BYTES || thumb.id == self.file.id {
                return Err(MessageError::Attachment);
            }
        }
        Ok(())
    }

    pub(super) fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        self.validate()?;
        self.file.encode(out);
        out.push(self.name.len() as u8);
        out.extend_from_slice(self.name.as_bytes());
        out.push(self.media_type.len() as u8);
        out.extend_from_slice(self.media_type.as_bytes());
        out.push(u8::from(self.thumbnail.is_some()));
        if let Some(thumb) = &self.thumbnail {
            thumb.encode(out);
        }
        Ok(())
    }

    pub(super) fn decode(input: &mut &[u8]) -> Result<Self> {
        fn string(input: &mut &[u8], max: usize) -> Result<String> {
            let (&len, rest) = input.split_first().ok_or(MessageError::Attachment)?;
            if len as usize > max {
                return Err(MessageError::Attachment);
            }
            let (value, rest) = rest
                .split_at_checked(len as usize)
                .ok_or(MessageError::Attachment)?;
            *input = rest;
            String::from_utf8(value.to_vec()).map_err(|_| MessageError::Attachment)
        }

        let file = BlobRef::decode(input)?;
        let name = string(input, MAX_NAME)?;
        let media_type = string(input, MAX_TYPE)?;
        let (&flag, rest) = input.split_first().ok_or(MessageError::Attachment)?;
        *input = rest;
        let thumbnail = match flag {
            0 => None,
            1 => Some(BlobRef::decode(input)?),
            _ => return Err(MessageError::Attachment),
        };
        let attachment = Self {
            file,
            name,
            media_type,
            thumbnail,
        };
        attachment.validate()?;
        Ok(attachment)
    }
}
