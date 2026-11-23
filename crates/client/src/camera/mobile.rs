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
    generation: u64,
    source: NativeVideoSource,
    latest: Mutex<(Instant, Option<I420Buffer>)>,
}
// The call controller joins the previous stop before starting another session.
static ACTIVE: Mutex<Option<Arc<Stream>>> = Mutex::new(None);

#[cfg(not(target_os = "android"))]
fn frame(buffer: I420Buffer) {
    frame_for_generation(buffer, None);
}

fn frame_for_generation(buffer: I420Buffer, generation: Option<u64>) {
    let Some(stream) = ACTIVE.lock().unwrap().clone() else {
        return;
    };
    if generation.is_some_and(|generation| generation != stream.generation) {
        return;
    }
    let frame = VideoFrame::new(VideoRotation::VideoRotation0, buffer);
    stream.source.capture_frame(&frame);
    *stream.latest.lock().unwrap() = (Instant::now(), Some(frame.buffer));
}

pub(super) struct Capture {
    pub source: NativeVideoSource,
    pub heights: Vec<u32>,
    pub mode: (u32, u32, u32),
    stream: Arc<Stream>,
    state: i32,
    pub multiple: bool,
    default_front: bool,
    control: Option<thread::JoinHandle<bool>>,
}

impl Capture {
    pub fn name(&self) -> &'static str {
        super::mobile_state(self.state).0
    }
    pub fn busy(&self) -> bool {
        self.control.is_some()
    }
    pub fn select(&mut self, name: Option<&str>) {
        debug_assert!(!self.busy());
        let front = name.map_or(self.default_front, |name| name != "back");
        self.control = Some(thread::spawn(move || platform::select(front)));
    }
    pub fn poll(&mut self) -> Option<bool> {
        let result = if self.control.as_ref().is_some_and(|job| job.is_finished()) {
            let ok = self.control.take().unwrap().join().unwrap_or(false);
            *self.stream.latest.lock().unwrap() = (Instant::now(), None);
            Some(ok)
        } else {
            None
        };
        if !self.busy() {
            let state = platform::state();
            // Keep the last selection if an interruption removed the active device.
            self.state = if state & 3 == 0 {
                self.state & 3
            } else {
                state
            };
        }
        result
    }
    pub fn torch(&self) -> Option<bool> {
        super::mobile_state(self.state).1
    }
    pub fn set_torch(&mut self, on: bool) {
        debug_assert!(!self.busy());
        self.control = Some(thread::spawn(move || platform::torch(on)));
    }
    pub fn is_finished(&self) -> bool {
        !self.busy()
            && (self.stream.latest.lock().unwrap().0.elapsed() > Duration::from_secs(5)
                || platform::ended())
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
        // Android cancellation wakes the pending JNI command without waiting on native callbacks.
        #[cfg(target_os = "android")]
        platform::stop();
        if let Some(job) = self.control.take() {
            let _ = job.join();
        }
        #[cfg(not(target_os = "android"))]
        platform::stop();
    }
}

pub(super) fn start(
    device: Device,
    options: Options,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<Capture> {
    let devices = platform::devices()?;
    let width = options.height * 16 / 9;
    let source = NativeVideoSource::new(
        VideoResolution {
            width,
            height: options.height,
        },
        false,
    );
    static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stream = Arc::new(Stream {
        generation,
        source: source.clone(),
        latest: Mutex::new((Instant::now(), None)),
    });
    *ACTIVE.lock().unwrap() = Some(stream.clone());
    let mut capture = Capture {
        source,
        stream,
        state: 0,
        multiple: devices & 3 == 3,
        default_front: devices & 1 != 0,
        control: None,
        mode: (width, options.height, options.fps),
        heights: vec![],
    };
    let cancelled = || cancelled.load(std::sync::atomic::Ordering::Acquire);
    ensure!(!cancelled(), "camera start cancelled");
    platform::start(
        device.name == "front",
        width,
        options.height,
        options.fps,
        generation,
    )?;
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
            capture.state = platform::state();
            return Ok(capture);
        }
        ensure!(
            !cancelled() && !capture.is_finished(),
            "camera stopped before first frame"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

// This fake checks Rust ownership and control completion; it does not validate hardware.
#[cfg(all(test, not(any(target_os = "android", target_os = "ios"))))]
mod platform {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::SeqCst};
    pub static STATE: AtomicI32 = AtomicI32::new(0);
    pub static DENIED: AtomicBool = AtomicBool::new(false);
    pub fn permission(_: bool) -> i32 {
        1
    }
    pub fn devices() -> anyhow::Result<i32> {
        Ok(3)
    }
    pub fn state() -> i32 {
        STATE.load(SeqCst)
    }
    pub fn start(front: bool, w: u32, h: u32, _: u32, _: u64) -> anyhow::Result<()> {
        STATE.store(if front { 1 } else { 6 }, SeqCst);
        super::frame(super::I420Buffer::new(w, h));
        Ok(())
    }
    pub fn select(front: bool) -> bool {
        if DENIED.load(SeqCst) {
            return false;
        }
        STATE.store(if front { 1 } else { 6 }, SeqCst);
        true
    }
    pub fn torch(on: bool) -> bool {
        if state() & 4 == 0 {
            return false;
        }
        STATE.store(if on { 14 } else { 6 }, SeqCst);
        true
    }
    pub fn ended() -> bool {
        state() == 0
    }
    pub fn stop() {
        STATE.store(0, SeqCst);
    }
}

#[cfg(all(test, not(any(target_os = "android", target_os = "ios"))))]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    fn settle(capture: &mut Capture) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(ok) = capture.poll() {
                return ok;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[tokio::test]
    async fn simulated_switch_failure_preview_and_stop_preserve_ownership() {
        assert_eq!(permission(false), 1);
        assert_eq!(list_cameras().unwrap().len(), 2);
        let mut capture = start(
            Device {
                name: "front".into(),
            },
            Options::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let stream = capture.stream.clone();
        let _publication = capture.source.clone();
        assert_eq!(capture.mode.1, Options::default().height);
        assert!(capture.heights.is_empty());
        assert!(capture.multiple);
        assert!(capture.take_preview().is_some());
        assert!(capture.take_preview().is_none());
        frame_for_generation(
            I420Buffer::new(16, 16),
            Some(stream.generation.wrapping_sub(1)),
        );
        assert!(
            capture.take_preview().is_none(),
            "late callbacks must not cross publications"
        );
        frame_for_generation(I420Buffer::new(16, 16), Some(stream.generation));
        assert!(capture.take_preview().is_some());
        assert_eq!(capture.name(), "front");
        assert_eq!(capture.torch(), None);
        platform::DENIED.store(true, SeqCst);
        capture.select(Some("back"));
        assert!(!settle(&mut capture));
        assert_eq!(capture.name(), "front");
        assert!(!capture.is_finished());
        assert!(Arc::ptr_eq(&stream, &capture.stream));
        platform::DENIED.store(false, SeqCst);
        capture.select(Some("back"));
        assert!(settle(&mut capture));
        assert_eq!(capture.torch(), Some(false));
        assert!(Arc::ptr_eq(&stream, &capture.stream));
        assert_eq!(capture.stream.generation, stream.generation);
        capture.set_torch(true);
        assert!(settle(&mut capture));
        assert_eq!(capture.torch(), Some(true));
        capture.select(Some("front"));
        assert!(settle(&mut capture));
        assert_eq!(capture.torch(), None);
        capture.select(Some("back"));
        assert!(settle(&mut capture));
        platform::stop();
        assert!(capture.is_finished());
        assert_eq!(capture.poll(), None);
        assert_eq!(capture.torch(), None);
        capture.stop().join().unwrap();
        let mut capture = start(
            Device {
                name: "back".into(),
            },
            Options::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_ne!(capture.stream.generation, stream.generation);
        capture.take_preview();
        frame_for_generation(I420Buffer::new(16, 16), Some(stream.generation));
        assert!(
            capture.take_preview().is_none(),
            "a stopped capture must not feed its replacement"
        );
        capture.set_torch(true);
        // Stop joins outstanding control work before releasing the device and preview.
        capture.stop().join().unwrap();
        assert_eq!(platform::state(), 0);
        assert!(ACTIVE.lock().unwrap().is_none());
        assert!(
            start(
                Device {
                    name: "front".into()
                },
                Options::default(),
                &AtomicBool::new(true)
            )
            .is_err()
        );
        assert!(ACTIVE.lock().unwrap().is_none());
    }
}
