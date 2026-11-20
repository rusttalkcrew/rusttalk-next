//! Pinned RustTalk connections, in-memory keys, and encrypted chat.

mod chat;
mod files;
mod keys;
mod server;
#[cfg(feature = "voice")]
pub mod voice;

pub use crate::keys::Event;
pub use crate::messages::{Attachment, BlobRef, MessageError};
pub use chat::{Catalog, Category, Channel, MAX_TEXT_CHARS, MessageBody, MessageRecord, RoleFlags};
pub use files::decode_image;
pub use keys::{KeyClient, invite_name};
pub use server::{Server, save_json};
