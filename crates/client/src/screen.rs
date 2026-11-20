//! Screen sharing: what to share, publishing it in a voice room, and showing the
//! screens other people share. Everything else uses these names.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use anyhow::{Result, bail};
use eframe::egui::{self, ColorImage};
use futures_util::StreamExt;
use livekit::{
    e2ee::EncryptionType,
    prelude::{RemoteTrackPublication, TrackKind},
    webrtc::{
        prelude::{BoxVideoFrame, RtcVideoTrack, VideoFormatType},
        video_stream::native::NativeVideoStream,
    },
};
use rusttalk_core::client::voice::{TrackSid, TrackSource, VoiceRoom};

use crate::audio::lock;

/// A screen or window the person can pick on Windows.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub id: String,
    pub name: String,
    pub window: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub struct Options {
    /// Largest picture height; `None` keeps the screen's own size.
    pub height: Option<u32>,
    pub fps: u32,
    pub cursor: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            height: Some(1080),
            fps: 30,
            cursor: true,
        }
    }
}

/// Windows lists screens and windows in the app; Linux asks the system's picker.
pub const SYSTEM_PICKER: bool = cfg!(target_os = "linux");
pub const SUPPORTED: bool = cfg!(any(target_os = "linux", windows));

/// A running capture. Dropping it stops capturing.
pub struct Capture {
    options: Options,
}

impl Capture {
    /// True if the shared window closed or the capture stopped by itself.
    pub fn is_finished(&self) -> bool {
        false
    }
}

// seam(w10-screen-share): `targets`, `capture`, `publish` and `name` call that branch's
// capture and publishing work once it is merged; the rest of the client is built on them.

/// Screens and windows to pick from (Windows). Starts OS work; keep it off the UI thread.
pub fn targets() -> Result<Vec<Target>> {
    bail!("screen capture is not available in this build")
}

/// Start capturing; `None` opens the system picker and waits for the person.
pub async fn capture(target: Option<Target>, options: Options) -> Result<Capture> {
    let _ = (target, Capture { options });
    bail!("screen capture is not available in this build")
}

/// Publish a capture as an encrypted screen track.
pub async fn publish(room: &VoiceRoom, capture: &Capture) -> Result<TrackSid> {
    let _ = (room, capture.options);
    bail!("screen capture is not available in this build")
}

/// The decrypted name of a remote screen share.
fn name(room: &VoiceRoom, publication: &RemoteTrackPublication) -> String {
    let _ = (room, publication);
    "Screen".into()
}

/// Receive a screen's video or stop receiving it.
pub fn set_subscribed(publication: &RemoteTrackPublication, on: bool) {
    publication.set_subscribed(on);
}

/// A remote screen share, if this publication is one we can decrypt: (track, name).
pub fn remote_screen(
    room: &VoiceRoom,
    publication: &RemoteTrackPublication,
) -> Option<(String, String)> {
    (publication.kind() == TrackKind::Video
        && publication.source() == TrackSource::Screenshare
        && publication.encryption_type() == EncryptionType::Gcm)
        .then(|| (publication.sid().to_string(), name(room, publication)))
}

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
                let Ok(image) = tokio::task::spawn_blocking(move || rgba(&frame)).await else {
                    break;
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

fn rgba(frame: &BoxVideoFrame) -> ColorImage {
    let (width, height) = (frame.buffer.width(), frame.buffer.height());
    let mut pixels = vec![0; width as usize * height as usize * 4];
    // libyuv names formats by word order: "ABGR" is R, G, B, A in memory.
    frame.buffer.to_argb(
        VideoFormatType::ABGR,
        &mut pixels,
        width * 4,
        width as i32,
        height as i32,
    );
    ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &pixels)
}
