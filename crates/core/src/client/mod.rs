//! Pinned RustTalk connections, in-memory keys, and encrypted chat.

pub mod backfill;
mod calendar;
mod chat;
mod files;
mod keys;
mod metadata;
pub mod revocations;
mod server;
pub mod sync;
#[cfg(feature = "voice")]
pub mod voice;

pub use crate::keys::Event;
pub use crate::messages::{
    Attachment, BlobRef, FriendSignal, KeyLink, LiveAttachment, MessageError,
};
pub use calendar::EventRefused;
pub use chat::{
    Catalog, Category, Channel, MAX_POKE_CHARS, MAX_TEXT_CHARS, MessageBody, MessageRecord,
    RoleFlags, SharedFile,
};
pub use files::{
    FileClient, Progress, UploadRejected, decode_frames, decode_frames_until, decode_image,
    decode_thumbnail, prepare_file,
};
pub use keys::{DmPin, KeyClient, Step, invite_name};
pub use metadata::{decode_profile_first_frame, decode_profile_image};
pub use server::{Server, save_json};
