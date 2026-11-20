//! Local application audio. Call list and start off the UI thread.

use anyhow::Result;
use serde::Deserialize;

#[cfg(target_os = "linux")]
#[path = "capture/linux.rs"]
mod platform;
#[cfg(windows)]
#[path = "capture/windows.rs"]
mod platform;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE / 100 * CHANNELS) as usize;

/// A currently playing local audio stream. The opaque ID is local to the OS.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct App {
    pub id: String,
    pub name: String,
}

/// The operating system does not provide per-application audio capture.
#[derive(Debug, thiserror::Error)]
#[error("application audio capture is not supported on this platform")]
pub struct Unsupported;

/// List playing applications. An unavailable backend returns an error.
pub fn list_apps() -> Result<Vec<App>> {
    #[cfg(any(target_os = "linux", windows))]
    return platform::list_apps();
    #[cfg(not(any(target_os = "linux", windows)))]
    Err(Unsupported.into())
}

/// Owns the capture. Dropping it requests shutdown without blocking the UI.
pub struct Capture {
    stop: Option<Box<dyn FnOnce() + Send>>,
    worker: std::thread::JoinHandle<()>,
}

impl Capture {
    /// True if the target disappeared or the backend stopped.
    pub fn is_finished(&self) -> bool {
        self.worker.is_finished()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

/// Deliver interleaved 48 kHz stereo s16 PCM in 10 ms frames on a capture worker.
/// The sink must return promptly. A final in-flight callback can finish after drop.
pub fn capture(app: &App, sink: impl FnMut(&[i16]) + Send + 'static) -> Result<Capture> {
    #[cfg(any(target_os = "linux", windows))]
    return platform::capture(app, sink);
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (app, sink);
        Err(Unsupported.into())
    }
}

#[cfg(any(target_os = "linux", windows))]
fn deliver(bytes: &[u8; FRAME_SAMPLES * 2], sink: &mut impl FnMut(&[i16])) {
    let mut frame = [0; FRAME_SAMPLES];
    for (sample, bytes) in frame.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *sample = i16::from_le_bytes(*bytes);
    }
    sink(&frame);
}

/// A stereo source with media processing disabled and no internal queue.
#[cfg(feature = "voice")]
pub fn audio_source() -> crate::client::voice::NativeAudioSource {
    crate::client::voice::NativeAudioSource::new(
        livekit::webrtc::audio_source::AudioSourceOptions::default(),
        SAMPLE_RATE,
        CHANNELS,
        0,
    )
}

/// Start app capture into a source created by `audio_source`.
#[cfg(feature = "voice")]
pub fn capture_to_source(
    app: &App,
    source: crate::client::voice::NativeAudioSource,
) -> Result<Capture> {
    use std::{
        borrow::Cow,
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use livekit::webrtc::audio_frame::AudioFrame;

    anyhow::ensure!(
        source.sample_rate() == SAMPLE_RATE && source.num_channels() == CHANNELS,
        "app capture requires a 48 kHz stereo source"
    );
    capture(app, move |samples| {
        let frame = AudioFrame {
            data: Cow::Borrowed(samples),
            sample_rate: SAMPLE_RATE,
            num_channels: CHANNELS,
            samples_per_channel: SAMPLE_RATE / 100,
        };
        match pin!(source.capture_frame(&frame)).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(Ok(())) => (),
            Poll::Ready(Err(error)) => tracing::warn!(%error, "sending app audio failed"),
            Poll::Pending => tracing::warn!("app audio source must have no internal queue"),
        }
    })
}
