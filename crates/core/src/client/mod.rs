//! Pinned RustTalk connections, in-memory keys, and encrypted chat.

mod chat;
mod keys;
mod server;

pub use crate::keys::Event;
pub use crate::messages::MessageError;
pub use chat::{Catalog, Category, Channel, MAX_TEXT_CHARS, MessageBody, MessageRecord, RoleFlags};
pub use keys::{KeyClient, invite_name};
pub use server::{Server, save_json};
