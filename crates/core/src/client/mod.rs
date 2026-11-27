//! Pinned RustTalk connections, in-memory keys, and encrypted chat.

pub mod backfill;
mod calendar;
mod chat;
mod files;
mod keys;
mod metadata;
mod reports;
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
    RoleFlags, SharedFile, SlowMode,
};
pub use files::{
    FileClient, Progress, UploadRejected, decode_frames, decode_frames_until, decode_image,
    decode_thumbnail, prepare_file,
};
pub use keys::{DmPin, KeyClient, Step, Verified, invite_name};
pub use metadata::{decode_profile_first_frame, decode_profile_image};
pub use reports::{
    AlreadyReported, Checked, ReceivedReport, RecipientsChanged, RecipientsUnchecked,
    ReportRefused, Unreadable, Visible, check_reviewers, recipients, removed_by,
};
pub use server::{Server, save_json};

use std::time::Duration;

/// The server asked to slow down (429) and when to try again.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("rate limited; try again in {0:?}")]
pub struct RateLimited(pub Duration);

impl RateLimited {
    /// From a 429 reply; one without a usable `Retry-After` waits a second.
    pub(crate) fn from_response(response: &reqwest::Response) -> Option<Self> {
        if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return None;
        }
        let seconds = (response.headers().get(reqwest::header::RETRY_AFTER))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(1);
        Some(Self(Duration::from_secs(seconds.clamp(1, 3600))))
    }

    /// Fail with `RateLimited` on a 429, otherwise as `error_for_status` does.
    pub(crate) fn check(response: reqwest::Response) -> anyhow::Result<reqwest::Response> {
        match Self::from_response(&response) {
            Some(limited) => Err(limited.into()),
            None => Ok(response.error_for_status()?),
        }
    }
}
