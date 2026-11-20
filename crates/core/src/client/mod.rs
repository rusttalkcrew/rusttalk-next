//! Pinned RustTalk connections, in-memory keys, and encrypted chat.

mod chat;
mod files;
mod keys;
mod metadata;
mod server;
pub mod sync;
#[cfg(feature = "voice")]
pub mod voice;

pub use crate::keys::Event;
pub use crate::messages::{Attachment, BlobRef, LiveAttachment, MessageError};
pub use chat::{Catalog, Category, Channel, MAX_TEXT_CHARS, MessageBody, MessageRecord, RoleFlags};
pub use files::{Progress, UploadRejected, decode_image, decode_thumbnail};
pub use keys::{KeyClient, invite_name};
pub use metadata::decode_profile_image;
pub use server::{Server, save_json};
