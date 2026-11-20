//! App audio sharing: the one place that talks to app capture and to app tracks
//! in a voice room. Everything else uses these names.

use std::{
    borrow::Cow,
    pin::pin,
    task::{Context, Poll, Waker},
};

use anyhow::{Result, bail};
use livekit::webrtc::{audio_frame::AudioFrame, audio_source::AudioSourceOptions};
use rusttalk_core::client::voice::{NativeAudioSource, RemoteAudioTrack, TrackSid, VoiceRoom};

const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;

// W09 seam: replace with `rusttalk_core::capture::{App, Capture}`.
/// An app that is playing sound right now.
#[derive(Clone, PartialEq)]
pub struct App {
    pub id: String,
    pub name: String,
}

pub struct Capture;

impl Capture {
    pub fn is_finished(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Support {
    Yes,
    /// Windows 10: show the button, disabled, with this reason.
    NeedsWindows11,
    No,
}

pub fn support() -> Support {
    // W09 seam: ask the capture module (Windows 10 is `NeedsWindows11`).
    if cfg!(any(target_os = "linux", windows)) {
        Support::Yes
    } else {
        Support::No
    }
}

/// Apps playing sound now. Called on the UI thread when the picker opens.
pub fn list() -> Result<Vec<App>> {
    // W09 seam: `rusttalk_core::capture::list_apps()`.
    Ok(Vec::new())
}

/// Start capturing an app into a new stereo source for its track.
pub fn capture(app: &App) -> Result<(Capture, NativeAudioSource)> {
    // Queue size 0: capture hands over exact 10 ms frames, nothing is buffered.
    let source = NativeAudioSource::new(AudioSourceOptions::default(), RATE, CHANNELS, 0);
    let sink = {
        let source = source.clone();
        move |frame: &[i16]| send(&source, frame)
    };
    // W09 seam: `rusttalk_core::capture::capture(app, sink)`.
    let _ = (app, sink);
    bail!("app capture is not available yet")
}

/// Publish a captured app as its own track; its name is encrypted for the room.
pub async fn publish(room: &VoiceRoom, app: &App, source: NativeAudioSource) -> Result<TrackSid> {
    // W09 seam: `room.publish_app_audio(&app.name, source).await?.sid()`.
    let _ = (room, app, source);
    bail!("app audio publishing is not available yet")
}

/// The app's name for a remote app track, or `None` for a microphone.
pub fn app_name(room: &VoiceRoom, track: &RemoteAudioTrack) -> Option<String> {
    // W09 seam: `room.app_audio_name(track)`.
    let _ = room;
    (track.source() == livekit::prelude::TrackSource::ScreenshareAudio).then(|| "App".into())
}

/// The source copies 10 ms frames synchronously when it has no queue.
fn send(source: &NativeAudioSource, frame: &[i16]) {
    let audio = AudioFrame {
        data: Cow::Borrowed(frame),
        sample_rate: RATE,
        num_channels: CHANNELS,
        samples_per_channel: frame.len() as u32 / CHANNELS,
    };
    let future = pin!(source.capture_frame(&audio));
    match future.poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(Ok(())) => {}
        Poll::Ready(Err(error)) => tracing::warn!(%error, "sending app audio failed"),
        Poll::Pending => tracing::warn!("app audio source is buffering"),
    }
}
