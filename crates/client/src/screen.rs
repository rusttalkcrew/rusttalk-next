//! Screen sharing: what to share and showing the screens other people share.
//! Capture and publishing live in `rusttalk_core::screen` and the voice room.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use eframe::egui::{self, ColorImage};
use futures_util::StreamExt;
use livekit::webrtc::{prelude::RtcVideoTrack, video_stream::native::NativeVideoStream};
use rusttalk_core::screen::frame_to_rgba;
pub use rusttalk_core::screen::{
    Cancelled, ScreenCapture as Capture, ScreenOptions as Options, ScreenSource as Target,
    ScreenSourceKind as Kind, list_sources as targets, start,
};

use crate::audio::lock;

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
                    let size = [
                        frame.buffer.width() as usize,
                        frame.buffer.height() as usize,
                    ];
                    frame_to_rgba(frame.buffer.as_ref())
                        .map(|pixels| ColorImage::from_rgba_unmultiplied(size, &pixels))
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
