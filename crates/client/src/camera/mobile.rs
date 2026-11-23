//! Phone cameras feed the same encrypted publication as desktop capture.
use super::Options;
use anyhow::{Result, ensure};
use livekit::webrtc::{
    video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation},
    video_source::{VideoResolution, native::NativeVideoSource},
};
use std::{
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "android")]
use android as platform;
#[cfg(target_os = "ios")]
mod ios;
#[cfg(target_os = "ios")]
use ios as platform;

pub(super) fn permission(request: bool) -> i32 {
    platform::permission(request)
}

pub(super) struct Device {
    pub name: String,
}
pub(super) fn list_cameras() -> Result<Vec<Device>> {
    let mask = platform::devices()?;
    Ok(["front", "back"]
        .into_iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, name)| Device { name: name.into() })
        .collect())
}

struct Stream {
    source: NativeVideoSource,
    latest: Mutex<(Instant, Option<I420Buffer>)>,
}
// The call controller joins the previous stop before starting another session.
static ACTIVE: Mutex<Option<Arc<Stream>>> = Mutex::new(None);

fn frame(buffer: I420Buffer) {
    let Some(stream) = ACTIVE.lock().unwrap().clone() else {
        return;
    };
    let frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
    stream.source.capture_frame(&frame);
    *stream.latest.lock().unwrap() = (Instant::now(), Some(frame.buffer));
}

pub(super) struct Capture {
    pub source: NativeVideoSource,
    pub heights: Vec<u32>,
    pub mode: (u32, u32, u32),
    stream: Arc<Stream>,
}

impl Capture {
    pub fn is_finished(&self) -> bool {
        self.stream.latest.lock().unwrap().0.elapsed() > Duration::from_secs(5) || platform::ended()
    }
    pub fn take_preview(&self) -> Option<image::RgbaImage> {
        let buffer = self.stream.latest.lock().unwrap().1.take()?;
        image::RgbaImage::from_raw(
            buffer.width(),
            buffer.height(),
            rusttalk_core::screen::frame_to_rgba(&buffer).ok()?,
        )
    }
    pub fn stop(self) -> thread::JoinHandle<()> {
        thread::spawn(move || drop(self))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        ACTIVE.lock().unwrap().take();
        platform::stop();
    }
}

pub(super) fn start(
    device: Device,
    options: Options,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Capture> {
    let width = options.height * 16 / 9;
    let source = NativeVideoSource::new(
        VideoResolution {
            width,
            height: options.height,
        },
        false,
    );
    let stream = Arc::new(Stream {
        source: source.clone(),
        latest: Mutex::new((Instant::now(), None)),
    });
    *ACTIVE.lock().unwrap() = Some(stream.clone());
    let mut capture = Capture {
        source,
        stream,
        mode: (width, options.height, options.fps),
        heights: vec![],
    };
    let cancelled = || cancelled.load(std::sync::atomic::Ordering::Acquire);
    ensure!(!cancelled(), "camera start cancelled");
    platform::start(device.name == "front", width, options.height, options.fps)?;
    loop {
        let size = capture
            .stream
            .latest
            .lock()
            .unwrap()
            .1
            .as_ref()
            .map(|b| (b.width(), b.height()));
        if let Some((w, h)) = size {
            capture.mode = (w, h, options.fps);
            return Ok(capture);
        }
        ensure!(
            !cancelled() && !capture.is_finished(),
            "camera stopped before first frame"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
