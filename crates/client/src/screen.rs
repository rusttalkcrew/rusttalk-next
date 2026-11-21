//! Screen sharing: what to share and showing the screens other people share.
//! Capture and publishing live in `rusttalk_core::screen` and the voice room.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use eframe::egui::{self, Color32, ColorImage};
use futures_util::StreamExt;
use livekit::webrtc::{prelude::RtcVideoTrack, video_stream::native::NativeVideoStream};
pub use rusttalk_core::screen::{
    Cancelled, ScreenCapture as Capture, ScreenOptions as Options, ScreenSource as Target,
    ScreenSourceKind as Kind, list_sources as targets, start, start_into,
};
use rusttalk_core::screen::{frame_bytes, frame_to_rgba_into};

use crate::audio::lock;

/// Changing what is shared while sharing: one picker at a time. The old share keeps
/// running through every state, so a cancelled or failed pick loses nothing.
/// `H` is the picker's task handle.
pub enum Swap<H> {
    Idle,
    Picking(H),
}

/// How a pick ended.
#[derive(Debug)]
pub enum Outcome<C> {
    /// The new source is capturing: use it in place of the old one.
    Swapped(C),
    /// The person closed the picker.
    Cancelled,
    Failed(anyhow::Error),
}

impl<H> Swap<H> {
    /// Start picking. Refused, handing the handle back, while a picker is open.
    pub fn begin(&mut self, picker: H) -> Result<(), H> {
        match self {
            Self::Idle => {
                *self = Self::Picking(picker);
                Ok(())
            }
            Self::Picking(_) => Err(picker),
        }
    }

    /// The picker's result. Back to idle whatever it was.
    pub fn finish<C>(&mut self, result: anyhow::Result<C>) -> Outcome<C> {
        *self = Self::Idle;
        match result {
            Ok(capture) => Outcome::Swapped(capture),
            Err(error) if error.is::<Cancelled>() => Outcome::Cancelled,
            Err(error) => Outcome::Failed(error),
        }
    }

    /// Stopping or leaving: forget the picker and give it back to close.
    pub fn abort(&mut self) -> Option<H> {
        match std::mem::replace(self, Self::Idle) {
            Self::Idle => None,
            Self::Picking(picker) => Some(picker),
        }
    }
}

pub const SUPPORTED: bool = cfg!(any(target_os = "linux", windows));

/// Watched screens, converted to RGBA off the UI thread. Only the newest frame is kept.
#[derive(Clone)]
pub struct Video {
    frames: Arc<Mutex<HashMap<String, Slot>>>,
    egui: egui::Context,
}

struct Slot {
    serial: u64,
    image: Option<ColorImage>,
}

impl Video {
    pub fn new(egui: egui::Context) -> Self {
        Self {
            frames: Arc::default(),
            egui,
        }
    }

    /// Show a subscribed track until it ends or is removed. Call on the network runtime.
    pub fn add_track(&self, sid: String, track: RtcVideoTrack) {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Replaces an older reader of the same track, which then stops.
        lock(&self.frames).insert(
            sid.clone(),
            Slot {
                serial,
                image: None,
            },
        );
        let video = self.clone();
        tokio::spawn(async move {
            // Holds one frame: a slow converter skips frames instead of falling behind.
            let mut stream = NativeVideoStream::new(track);
            while let Some(frame) = stream.next().await {
                let converted = tokio::task::spawn_blocking(move || {
                    let (width, height) = (frame.buffer.width(), frame.buffer.height());
                    frame_bytes(width, height)?;
                    // Decoded straight into the image: 1080p frames used to take ~4 ms
                    // more for a second, per-pixel copy.
                    let mut image =
                        ColorImage::filled([width as usize, height as usize], Color32::BLACK);
                    frame_to_rgba_into(frame.buffer.as_ref(), image.as_raw_mut())?;
                    anyhow::Ok(image)
                })
                .await;
                let image = match converted {
                    Ok(Ok(image)) => image,
                    Ok(Err(error)) => {
                        tracing::debug!(%error, "skipped a video frame");
                        continue;
                    }
                    Err(_) => break,
                };
                let mut frames = lock(&video.frames);
                let Some(slot) = frames.get_mut(&sid).filter(|s| s.serial == serial) else {
                    return;
                };
                slot.image = Some(image);
                drop(frames);
                video.egui.request_repaint();
            }
            let mut frames = lock(&video.frames);
            if frames.get(&sid).is_some_and(|s| s.serial == serial) {
                frames.remove(&sid);
            }
        });
    }

    pub fn remove_track(&self, sid: &str) {
        lock(&self.frames).remove(sid);
    }

    /// The newest frame since the last call, to upload as a texture.
    pub fn take(&self, sid: &str) -> Option<ColorImage> {
        lock(&self.frames).get_mut(sid)?.image.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_walks_through_picking() {
        let mut swap = Swap::Idle;
        assert!(swap.begin(1).is_ok());
        // A second picker is refused while one is open.
        assert_eq!(swap.begin(2), Err(2));
        assert!(matches!(swap.finish(Ok("new")), Outcome::Swapped("new")));
        assert!(matches!(swap, Swap::Idle));
        // Closed without choosing: keep the old share.
        swap.begin(3).unwrap();
        let closed: anyhow::Result<()> = Err(Cancelled.into());
        assert!(matches!(swap.finish(closed), Outcome::Cancelled));
        assert!(matches!(swap, Swap::Idle));
        // Failed to start: reported, and a new pick can begin.
        swap.begin(4).unwrap();
        let failed: anyhow::Result<()> = Err(anyhow::anyhow!("no frames"));
        assert!(matches!(swap.finish(failed), Outcome::Failed(_)));
        assert!(swap.begin(5).is_ok());
    }

    #[test]
    fn stopping_closes_the_picker() {
        let mut swap: Swap<u8> = Swap::Idle;
        assert_eq!(swap.abort(), None);
        swap.begin(7).unwrap();
        assert_eq!(swap.abort(), Some(7));
        assert!(matches!(swap, Swap::Idle));
    }
}
