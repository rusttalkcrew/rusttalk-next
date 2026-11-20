//! App audio sharing: the one place that talks to app capture and to app tracks
//! in a voice room. Everything else uses these names.

use anyhow::Result;
use rusttalk_core::{
    capture,
    client::voice::{NativeAudioSource, RemoteAudioTrack, TrackSid, VoiceRoom},
};

pub use capture::{App, Capture};

#[derive(Clone, Copy, PartialEq)]
pub enum Support {
    Yes,
    /// Windows 10: show the button, disabled, with this reason.
    NeedsWindows11,
    No,
}

pub fn support() -> Support {
    match capture::supported() {
        true => Support::Yes,
        false if cfg!(windows) => Support::NeedsWindows11,
        false => Support::No,
    }
}

/// Apps playing sound now. Starts a helper process on Linux; keep it off the UI thread.
pub fn list() -> Result<Vec<App>> {
    capture::list_apps()
}

/// Start capturing an app into a new source for its track. Blocks until audio flows.
pub fn capture(app: &App) -> Result<(Capture, NativeAudioSource)> {
    let source = capture::audio_source();
    let capture = capture::capture_to_source(app, source.clone())?;
    Ok((capture, source))
}

/// Publish a captured app as its own track; its name is encrypted for the room.
pub async fn publish(room: &VoiceRoom, app: &App, source: NativeAudioSource) -> Result<TrackSid> {
    Ok(room.publish_app_audio(&app.name, source).await?.sid())
}

/// The app's name for a remote app track, or `None` for a microphone.
pub fn app_name(room: &VoiceRoom, track: &RemoteAudioTrack) -> Option<String> {
    room.app_audio_name(track)
}
