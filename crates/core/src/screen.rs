//! Screen selection, capture and pixel conversion for native video tracks.

use std::{
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant as StdInstant},
};

use anyhow::{Context, Result, ensure};
use livekit::webrtc::{
    native::yuv_helper::{abgr_to_i420, argb_to_i420, i420_to_abgr},
    video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation},
    video_source::{VideoResolution, native::NativeVideoSource},
};
use tokio::{
    task::JoinHandle,
    time::{Instant, sleep_until, timeout},
};

#[cfg(windows)]
#[path = "screen/input_windows.rs"]
mod input;
#[cfg(target_os = "linux")]
#[path = "screen/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "screen/macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "screen/windows.rs"]
mod platform;

/// Whether this system can put a remote controller's input into a share. Linux needs
/// the desktop portal's RemoteDesktop session, which is not wired up yet.
pub const REMOTE_CONTROL: bool = cfg!(windows);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenSourceKind {
    Portal,
    Monitor,
    Window,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenSource {
    pub name: String,
    pub kind: ScreenSourceKind,
    #[cfg(any(windows, target_os = "macos"))]
    target: platform::Target,
}

/// Preserve aspect ratio and only scale down. None uses the captured resolution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScreenOptions {
    pub max_height: Option<u32>,
    pub fps: u32,
    pub cursor: bool,
}

impl Default for ScreenOptions {
    fn default() -> Self {
        Self {
            max_height: Some(1080),
            fps: 30,
            cursor: true,
        }
    }
}

impl ScreenOptions {
    pub(crate) fn validate(self) -> Result<()> {
        ensure!(
            matches!(self.fps, 5 | 15 | 30 | 60),
            "invalid screen frame rate"
        );
        ensure!(
            matches!(self.max_height, None | Some(720 | 1080 | 1440)),
            "invalid screen resolution cap"
        );
        Ok(())
    }

    fn period(self) -> Duration {
        Duration::from_secs(1) / self.fps
    }
}

/// Windows and macOS enumerate the user's monitors and windows.
/// Linux offers one entry that opens the desktop's picker.
pub fn list_sources() -> Result<Vec<ScreenSource>> {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    return platform::list_sources();
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    anyhow::bail!("screen capture is unavailable on this platform")
}

/// The person closed the system screen picker without choosing.
#[derive(Debug, thiserror::Error)]
#[error("screen picker cancelled")]
pub struct Cancelled;

struct BgraFrame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// The RGBA size of a frame, refused when out of range: check before allocating one.
pub fn frame_bytes(width: u32, height: u32) -> Result<usize> {
    ensure!(
        (1..=8192).contains(&width)
            && (1..=8192).contains(&height)
            && width * height <= 7680 * 4320,
        "screen frame dimensions out of range"
    );
    Ok((width * height * 4) as usize)
}

type PackedToI420 = fn(&[u8], u32, &mut [u8], u32, &mut [u8], u32, &mut [u8], u32, i32, i32);

fn packed_to_i420(
    width: u32,
    height: u32,
    pixels: &[u8],
    convert: PackedToI420,
) -> Result<I420Buffer> {
    ensure!(
        pixels.len() == frame_bytes(width, height)?,
        "invalid packed frame length"
    );
    let mut frame = I420Buffer::new(width, height);
    let (ys, us, vs) = frame.strides();
    let (y, u, v) = frame.data_mut();
    convert(
        pixels,
        width * 4,
        y,
        ys,
        u,
        us,
        v,
        vs,
        width as i32,
        height as i32,
    );
    Ok(frame)
}

/// Convert tightly packed RGBA (R, G, B, A in memory) to I420.
pub(crate) fn rgba_to_i420(width: u32, height: u32, pixels: &[u8]) -> Result<I420Buffer> {
    // Libyuv names formats by word order: ABGR is R, G, B, A in memory.
    packed_to_i420(width, height, pixels, abgr_to_i420)
}

/// Convert tightly packed BGRA/BGRx to I420 and optionally scale down.
pub fn bgra_to_i420(
    width: u32,
    height: u32,
    pixels: &[u8],
    max_height: Option<u32>,
) -> Result<I420Buffer> {
    let mut frame = packed_to_i420(width, height, pixels, argb_to_i420)?;
    if let Some(cap) = max_height.filter(|cap| *cap < height) {
        ensure!(cap > 0, "invalid screen resolution cap");
        let scaled_width = (width * cap / height).max(1);
        frame = frame.scale(scaled_width as i32, cap as i32);
    }
    Ok(frame)
}

/// Where a frame of `source` size sits in a `canvas`: the biggest even size of its shape
/// that fits, and its even offset, so the chroma planes stay aligned. (width, height, x, y)
pub(crate) fn fit(source: (u32, u32), canvas: (u32, u32)) -> (u32, u32, u32, u32) {
    let ((sw, sh), (cw, ch)) = (source, canvas);
    if source == canvas {
        return (cw, ch, 0, 0);
    }
    let (w, h) = if u64::from(sw) * u64::from(ch) <= u64::from(sh) * u64::from(cw) {
        (u64::from(sw) * u64::from(ch) / u64::from(sh), u64::from(ch))
    } else {
        (u64::from(cw), u64::from(sh) * u64::from(cw) / u64::from(sw))
    };
    let even = |n: u64, max: u32| ((n as u32).max(2) & !1).min(max & !1).max(1);
    let (w, h) = (even(w, cw), even(h, ch));
    (w, h, ((cw - w) / 2) & !1, ((ch - h) / 2) & !1)
}

/// Fit a frame into a canvas of a track that is already published: scale it to fit and
/// pad the rest with black. The published size never changes, so viewers need no new
/// negotiation when the source changes to one of another size.
pub(crate) fn letterbox(frame: I420Buffer, canvas: (u32, u32)) -> I420Buffer {
    let mut frame = frame;
    let (w, h, x, y) = fit((frame.width(), frame.height()), canvas);
    if (frame.width(), frame.height()) == canvas {
        return frame;
    }
    let scaled = if (frame.width(), frame.height()) == (w, h) {
        frame
    } else {
        frame.scale(w as i32, h as i32)
    };
    let mut out = I420Buffer::new_black(canvas.0, canvas.1);
    let (ss, sus, svs) = scaled.strides();
    let (sy, su, sv) = scaled.data();
    let (os, ous, ovs) = out.strides();
    let (oy, ou, ov) = out.data_mut();
    let copy = |from: &[u8],
                from_stride: u32,
                to: &mut [u8],
                to_stride: u32,
                rows: u32,
                row: usize,
                left: usize,
                width: usize| {
        for r in 0..rows as usize {
            let a = r * from_stride as usize;
            let b = (row + r) * to_stride as usize + left;
            to[b..b + width].copy_from_slice(&from[a..a + width]);
        }
    };
    let (x, y) = (x as usize, y as usize);
    copy(
        sy,
        ss,
        oy,
        os,
        scaled.height(),
        y,
        x,
        scaled.width() as usize,
    );
    let (cw, ch) = (scaled.chroma_width(), scaled.chroma_height());
    copy(su, sus, ou, ous, ch, y / 2, x / 2, cw as usize);
    copy(sv, svs, ov, ovs, ch, y / 2, x / 2, cw as usize);
    out
}

/// Convert a decoded frame into tightly packed RGBA for display.
pub fn frame_to_rgba(frame: &dyn VideoBuffer) -> Result<Vec<u8>> {
    let mut rgba = vec![0; frame_bytes(frame.width(), frame.height())?];
    frame_to_rgba_into(frame, &mut rgba)?;
    Ok(rgba)
}

/// Like `frame_to_rgba`, into a buffer of exactly `frame_bytes`, such as an egui image:
/// that saves copying every received frame once more.
pub fn frame_to_rgba_into(frame: &dyn VideoBuffer, rgba: &mut [u8]) -> Result<()> {
    let (width, height) = (frame.width(), frame.height());
    ensure!(
        rgba.len() == frame_bytes(width, height)?,
        "RGBA buffer does not match the frame size"
    );
    let converted;
    let frame = if let Some(i420) = frame.as_i420() {
        i420
    } else {
        converted = frame.to_i420();
        &converted
    };
    let (ys, us, vs) = frame.strides();
    let (y, u, v) = frame.data();
    // Libyuv names formats by word order: ABGR is R, G, B, A in memory.
    i420_to_abgr(
        y,
        ys,
        u,
        us,
        v,
        vs,
        rgba,
        width * 4,
        width as i32,
        height as i32,
    );
    Ok(())
}

/// The preview's limits: small and slow enough to cost the share next to nothing.
const PREVIEW_WIDTH: u32 = 960;
const PREVIEW_PERIOD: Duration = Duration::from_millis(1000 / 15);

/// A frame as sent to the encoder, shared so the preview can read it without the
/// capture task copying it.
struct Shared(Arc<I420Buffer>);

impl AsRef<dyn VideoBuffer> for Shared {
    fn as_ref(&self) -> &(dyn VideoBuffer + 'static) {
        &*self.0
    }
}

/// The sharer's own view of their share: a small, low-rate copy of the captured
/// frames, taken before encoding, so it needs no round trip through the server.
/// The capture only hands frames over; a worker thread shrinks them. While off, the
/// capture checks one flag per frame.
#[derive(Clone, Default)]
pub struct Preview(Arc<PreviewState>);

#[derive(Default)]
struct PreviewState {
    on: AtomicBool,
    started: AtomicBool,
    /// A frame for the worker to shrink, and when the capture last handed one over.
    raw: Mutex<(Option<Arc<I420Buffer>>, Option<StdInstant>)>,
    wake: Condvar,
    /// The newest small copy, until the window takes it.
    small: Mutex<Option<I420Buffer>>,
    /// `small` holds a copy: the capture skips frames until the window took it.
    waiting: AtomicBool,
}

impl Preview {
    pub fn set(&self, on: bool) {
        self.0.on.store(on, Relaxed);
        if on && !self.0.started.swap(true, Relaxed) {
            let state = Arc::downgrade(&self.0);
            let spawned = std::thread::Builder::new()
                .name("screen-preview".into())
                .spawn(move || shrink_frames(state));
            if let Err(error) = spawned {
                tracing::warn!(%error, "could not start the screen preview");
            }
        }
        if !on {
            if let Ok(mut raw) = self.0.raw.lock() {
                *raw = (None, None);
            }
            if let Ok(mut small) = self.0.small.lock() {
                *small = None;
            }
            self.0.waiting.store(false, Relaxed);
        }
    }

    pub fn is_on(&self) -> bool {
        self.0.on.load(Relaxed)
    }

    /// The newest copy since the last call. Never waits for the worker.
    pub fn take(&self) -> Option<I420Buffer> {
        let frame = self.0.small.try_lock().ok()?.take();
        if frame.is_some() {
            self.0.waiting.store(false, Relaxed);
        }
        frame
    }

    /// Called by the capture after each frame went to the encoder. It never waits
    /// and never copies pixels: a frame is skipped while the window hasn't taken the
    /// last copy, the worker is busy or holds the slot, or the last hand-over is
    /// younger than the preview's frame period.
    fn offer(&self, frame: &Arc<I420Buffer>, now: StdInstant) {
        if !self.is_on() || self.0.waiting.load(Relaxed) {
            return;
        }
        let Ok(mut raw) = self.0.raw.try_lock() else {
            return;
        };
        if raw.0.is_some() || raw.1.is_some_and(|last| now < last + PREVIEW_PERIOD) {
            return;
        }
        *raw = (Some(frame.clone()), Some(now));
        drop(raw);
        self.0.wake.notify_one();
    }
}

/// The preview's worker: shrinks what the capture hands over. Ends once every
/// `Preview` handle is gone.
fn shrink_frames(state: Weak<PreviewState>) {
    loop {
        let Some(state) = state.upgrade() else {
            return;
        };
        let frame = {
            let Ok(raw) = state.raw.lock() else {
                return;
            };
            let idle = |raw: &mut (Option<Arc<I420Buffer>>, Option<StdInstant>)| raw.0.is_none();
            let Ok((mut raw, _)) = state
                .wake
                .wait_timeout_while(raw, Duration::from_secs(1), idle)
            else {
                return;
            };
            raw.0.take()
        };
        let Some(frame) = frame else {
            continue;
        };
        // libwebrtc only scales a buffer it may write to: copy it, unless the capture
        // already moved on to a newer one.
        let mut frame =
            Arc::try_unwrap(frame).unwrap_or_else(|frame| (&*frame as &dyn VideoBuffer).to_i420());
        let small = shrink(&mut frame);
        if state.on.load(Relaxed)
            && let Ok(mut slot) = state.small.lock()
        {
            *slot = Some(small);
            state.waiting.store(true, Relaxed);
        }
    }
}

/// At most `PREVIEW_WIDTH` wide, keeping its shape.
fn shrink(frame: &mut I420Buffer) -> I420Buffer {
    let (width, height) = (frame.width(), frame.height());
    let scaled = width.min(PREVIEW_WIDTH);
    frame.scale(scaled as i32, (height * scaled / width).max(1) as i32)
}

/// A screen source owns capture until dropped. Dropping it requests shutdown.
pub struct ScreenCapture {
    pub name: String,
    pub source: NativeVideoSource,
    task: JoinHandle<()>,
    #[cfg(windows)]
    target: input::Target,
    /// The published size, when frames are letterboxed into an earlier share's track.
    canvas: Option<(u32, u32)>,
    /// The newest captured frame's size, packed as width << 32 | height.
    size: Arc<AtomicU64>,
}

impl ScreenCapture {
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Puts a remote controller's input into this share. It only acts where
    /// `REMOTE_CONTROL` is true.
    pub fn remote(&self) -> Remote {
        Remote {
            #[cfg(windows)]
            target: self.target,
            canvas: self.canvas,
            size: self.size.clone(),
        }
    }
}

/// Puts a remote controller's input into what is shared, and nowhere else.
pub struct Remote {
    #[cfg(windows)]
    target: input::Target,
    canvas: Option<(u32, u32)>,
    size: Arc<AtomicU64>,
}

impl Remote {
    /// Where a point on the published picture (0 to 1 each way) is on the captured
    /// screen or window; `None` on the black bars of a letterboxed share.
    pub fn point(&self, x: f32, y: f32) -> Option<(f32, f32)> {
        let size = self.size.load(Relaxed);
        to_source((x, y), ((size >> 32) as u32, size as u32), self.canvas)
    }

    /// Inject one validated input event. Positions outside the share, and keys while
    /// something else has the focus, are dropped.
    pub fn inject(&self, event: &crate::control::Input) -> Result<()> {
        ensure!(event.valid(), "invalid remote input");
        #[cfg(windows)]
        return input::inject(self.target, event, |x, y| self.point(x, y));
        #[cfg(not(windows))]
        anyhow::bail!("remote control is not supported on this system yet")
    }
}

/// Undo `letterbox`: a point on the canvas (0 to 1) as a point on the source frame.
fn to_source(
    point: (f32, f32),
    source: (u32, u32),
    canvas: Option<(u32, u32)>,
) -> Option<(f32, f32)> {
    let Some(canvas) = canvas else {
        return Some(point);
    };
    if source.0 == 0 || source.1 == 0 {
        return None;
    }
    let (w, h, x, y) = fit(source, canvas);
    let sx = (point.0 * canvas.0 as f32 - x as f32) / w as f32;
    let sy = (point.1 * canvas.1 as f32 - y as f32) / h as f32;
    ((0.0..=1.0).contains(&sx) && (0.0..=1.0).contains(&sy)).then_some((sx, sy))
}

fn pack(frame: &BgraFrame) -> u64 {
    u64::from(frame.width) << 32 | u64::from(frame.height)
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Wait for the first frame and start feeding a screencast source at the selected rate.
/// The Linux portal can return a new single-use restore token. Store the updated
/// option even if capture fails after the picker. Pass None to open a fresh picker.
pub async fn start(
    source: ScreenSource,
    options: ScreenOptions,
    restore_token: &mut Option<String>,
    preview: Preview,
) -> Result<ScreenCapture> {
    start_into(source, options, restore_token, None, preview).await
}

/// Like `start`, but feeds frames into the source of a track that is already published,
/// fitted to its size, so viewers keep watching while the screen being shared changes.
/// Drop the old capture once this one is returned.
pub async fn start_into(
    source: ScreenSource,
    options: ScreenOptions,
    restore_token: &mut Option<String>,
    into: Option<NativeVideoSource>,
    preview: Preview,
) -> Result<ScreenCapture> {
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        options.validate()?;
        #[cfg(windows)]
        let target = input::Target::from(source.target);
        let (name, mut frames, backend) = platform::open(source, options, restore_token).await?;
        timeout(Duration::from_secs(8), frames.changed())
            .await
            .context("timed out waiting for the first screen frame")?
            .context("screen capture ended before supplying video")?;
        let canvas = into.as_ref().map(|into| {
            let size = into.video_resolution();
            (size.width, size.height)
        });
        let convert = move |frame: &BgraFrame| {
            let frame = bgra_to_i420(frame.width, frame.height, &frame.pixels, options.max_height)?;
            Ok::<_, anyhow::Error>(match canvas {
                Some(canvas) => letterbox(frame, canvas),
                None => frame,
            })
        };
        let size = Arc::new(AtomicU64::new(0));
        let first = {
            let frame = frames.borrow_and_update();
            let frame = frame.as_ref().context("missing first screen frame")?;
            size.store(pack(frame), Relaxed);
            convert(frame)?
        };
        let source = into.unwrap_or_else(|| {
            NativeVideoSource::new(
                VideoResolution {
                    width: first.width(),
                    height: first.height(),
                },
                true,
            )
        });
        // Seed the source before publication. Reuse converted pixels for static content.
        let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, Shared(Arc::new(first)));
        source.capture_frame(&frame);
        let feeder = source.clone();
        let sizes = size.clone();
        let task = tokio::spawn(async move {
            let period = options.period();
            let mut next = Instant::now();
            loop {
                // Send each captured frame as it arrives. Resampling on a timer of the
                // same rate repeated and skipped frames (judder) and added up to a period
                // of delay. A still screen repeats its last frame every period or so.
                let changed = timeout(period, frames.changed()).await;
                if backend.is_finished() {
                    tracing::warn!("screen capture ended");
                    break;
                }
                match changed {
                    Ok(Ok(())) => {
                        let pending = frames.borrow_and_update();
                        if let Some(next) = pending.as_ref() {
                            sizes.store(pack(next), Relaxed);
                            match convert(next) {
                                Ok(next) => frame.buffer = Shared(Arc::new(next)),
                                Err(error) => {
                                    tracing::warn!(%error, "could not convert captured screen frame");
                                    break;
                                }
                            }
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(_) => (),
                }
                feeder.capture_frame(&frame);
                preview.offer(&frame.buffer.0, StdInstant::now());
                // Thin a faster capture to the chosen rate, on a steady schedule that lets
                // a frame come a quarter period early (as the PipeWire helper does).
                next = next.max(Instant::now() - period / 2) + period;
                sleep_until(next - period / 4).await;
            }
        });
        Ok(ScreenCapture {
            name,
            source,
            task,
            #[cfg(windows)]
            target,
            canvas,
            size,
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (source, options, restore_token, into, preview);
        anyhow::bail!("screen capture is unavailable on this platform")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_bounds() {
        assert_eq!(frame_bytes(3, 5).unwrap(), 60);
        assert_eq!(frame_bytes(7680, 4320).unwrap(), 7680 * 4320 * 4);
        for (width, height) in [(0, 1), (1, 0), (8193, 1), (1, 8193), (8192, 8192)] {
            assert!(frame_bytes(width, height).is_err());
        }
    }

    #[test]
    fn odd_frames_convert() {
        // Red in BGRA memory order, with odd dimensions for the chroma planes.
        let pixels = [0, 0, 255, 255].repeat(5 * 3);
        assert!(bgra_to_i420(5, 3, &pixels[4..], None).is_err());
        assert!(bgra_to_i420(5, 3, &[pixels.clone(), vec![0; 4]].concat(), None).is_err());
        assert!(bgra_to_i420(0, 3, &[], None).is_err());
        let frame = bgra_to_i420(5, 3, &pixels, None).unwrap();
        let rgba = frame_to_rgba(&frame).unwrap();
        assert_eq!(rgba.len(), 5 * 3 * 4);
        let mut wrong = vec![0; 5 * 3 * 4 - 4];
        assert!(frame_to_rgba_into(&frame, &mut wrong).is_err());
        assert!(rgba[0] > 200 && rgba[1] < 50 && rgba[2] < 50);
        // The same red read back through the RGBA path used for cameras.
        let again = frame_to_rgba(&rgba_to_i420(5, 3, &rgba).unwrap()).unwrap();
        assert!(again[0] > 200 && again[1] < 50 && again[2] < 50);
        assert!(rgba_to_i420(5, 3, &rgba[4..]).is_err());
        let scaled = bgra_to_i420(5, 3, &pixels, Some(1)).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (1, 1));
        assert_eq!(frame_to_rgba(&scaled).unwrap().len(), 4);
        let pixels = [0; 4].repeat(7 * 1081);
        let scaled = bgra_to_i420(7, 1081, &pixels, Some(1080)).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (6, 1080));
    }

    #[test]
    fn quality_steps() {
        for max_height in [Some(720), Some(1080), Some(1440), None] {
            let options = ScreenOptions {
                max_height,
                ..ScreenOptions::default()
            };
            assert!(options.validate().is_ok());
        }
        let odd = ScreenOptions {
            max_height: Some(900),
            ..ScreenOptions::default()
        };
        assert!(odd.validate().is_err());
        // 1440p scales a bigger screen down to fit, keeping its shape...
        let pixels = [0; 4].repeat(2560 * 1600);
        let scaled = bgra_to_i420(2560, 1600, &pixels, Some(1440)).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (2304, 1440));
        // ...and leaves a smaller screen as it is.
        let pixels = [0; 4].repeat(1920 * 1080);
        let same = bgra_to_i420(1920, 1080, &pixels, Some(1440)).unwrap();
        assert_eq!((same.width(), same.height()), (1920, 1080));
    }

    fn i420(width: u32, height: u32) -> Arc<I420Buffer> {
        let pixels = [0; 4].repeat((width * height) as usize);
        Arc::new(bgra_to_i420(width, height, &pixels, None).unwrap())
    }

    /// The worker's next copy, waiting for it at most a few seconds.
    fn next_copy(preview: &Preview) -> Option<I420Buffer> {
        for _ in 0..500 {
            if let Some(copy) = preview.take() {
                return Some(copy);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    fn handed_over(preview: &Preview) -> Option<StdInstant> {
        preview.0.raw.lock().unwrap().1
    }

    #[test]
    fn preview_taps_a_small_copy_without_waiting() {
        let preview = Preview::default();
        let frame = i420(1920, 1080);
        let start = StdInstant::now();
        // Off: nothing is handed over.
        preview.offer(&frame, start);
        assert_eq!(handed_over(&preview), None);
        preview.set(true);
        preview.offer(&frame, start);
        let copy = next_copy(&preview).unwrap();
        assert_eq!((copy.width(), copy.height()), (960, 540));
        // Offered more often than the preview's rate: skipped.
        preview.offer(&frame, start + PREVIEW_PERIOD / 2);
        assert_eq!(handed_over(&preview), Some(start));
        // A slow window: frames are skipped until it took the last copy.
        let later = start + PREVIEW_PERIOD;
        preview.offer(&frame, later);
        while !preview.0.waiting.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
        preview.offer(&frame, later + PREVIEW_PERIOD * 2);
        assert_eq!(handed_over(&preview), Some(later));
        assert!(preview.take().is_some());
        // The capture never waits for a worker that holds the slot.
        let held = preview.0.raw.lock().unwrap();
        preview.offer(&frame, later + PREVIEW_PERIOD * 3);
        drop(held);
        assert_eq!(handed_over(&preview), Some(later));
        // Turning it off drops what wasn't shown yet and stops the hand-overs.
        preview.set(false);
        assert!(preview.take().is_none());
        preview.offer(&frame, later + PREVIEW_PERIOD * 4);
        assert_eq!(handed_over(&preview), None);
        // Small screens are copied as they are.
        preview.set(true);
        preview.offer(&i420(640, 360), start);
        let copy = next_copy(&preview).unwrap();
        assert_eq!((copy.width(), copy.height()), (640, 360));
    }

    #[test]
    fn preview_costs_the_capture_little_at_4k() {
        let preview = Preview::default();
        preview.set(true);
        let mut now = StdInstant::now();
        let mut took = Duration::ZERO;
        let rounds = 30;
        for _ in 0..rounds {
            // A fresh frame each time, as the capture sends.
            let frame = i420(3840, 2160);
            let offered = Instant::now();
            preview.offer(&frame, now);
            took += offered.elapsed();
            drop(frame);
            assert!(next_copy(&preview).is_some());
            now += PREVIEW_PERIOD;
        }
        let each = took / rounds;
        println!("capture-side cost per 4K frame handed to the preview: {each:?}");
        assert!(each < Duration::from_millis(1), "{each:?}");
    }

    #[test]
    fn frames_fit_the_published_size() {
        // Same shape: fills it. Wider: bars above and below. Taller: bars at the sides.
        assert_eq!(fit((1920, 1080), (1920, 1080)), (1920, 1080, 0, 0));
        assert_eq!(fit((1280, 720), (1920, 1080)), (1920, 1080, 0, 0));
        assert_eq!(fit((2560, 1080), (1920, 1080)), (1920, 810, 0, 134));
        assert_eq!(fit((1080, 1920), (1920, 1080)), (606, 1080, 656, 0));
        // Odd sizes stay even and inside the canvas.
        let (w, h, x, y) = fit((7, 1081), (1919, 1079));
        assert!(w % 2 == 0 && h % 2 == 0 && x % 2 == 0 && y % 2 == 0);
        assert!(w + x <= 1919 && h + y <= 1079);
    }

    #[test]
    fn remote_points_map_to_the_source() {
        // Not letterboxed: the picture is the source.
        assert_eq!(
            to_source((0.25, 1.0), (1920, 1080), None),
            Some((0.25, 1.0))
        );
        // A 4:3 window shown in a 16:9 track: bars at the sides.
        let canvas = Some((1920, 1080));
        let (w, _, x, _) = fit((1440, 1080), (1920, 1080));
        assert_eq!((w, x), (1440, 240));
        assert_eq!(
            to_source((0.5, 0.5), (1440, 1080), canvas),
            Some((0.5, 0.5))
        );
        let left_edge = 240.0 / 1920.0;
        assert_eq!(
            to_source((left_edge, 0.0), (1440, 1080), canvas),
            Some((0.0, 0.0))
        );
        // On the bars: nothing to click.
        assert_eq!(to_source((0.05, 0.5), (1440, 1080), canvas), None);
        assert_eq!(to_source((0.95, 0.5), (1440, 1080), canvas), None);
        // No frame yet.
        assert_eq!(to_source((0.5, 0.5), (0, 0), canvas), None);
    }

    #[test]
    fn letterbox_keeps_the_canvas_size() {
        // A bright square source, narrower than the canvas, ends up centred on black.
        let pixels = [255; 4].repeat(100 * 100);
        let source = bgra_to_i420(100, 100, &pixels, None).unwrap();
        let out = letterbox(source, (200, 100));
        assert_eq!((out.width(), out.height()), (200, 100));
        let (y, _, _) = out.data();
        let stride = out.strides().0 as usize;
        assert!(y[50 * stride] < 30, "left bar is black");
        assert!(y[50 * stride + 100] > 200, "middle is the picture");
        assert!(y[50 * stride + 199] < 30, "right bar is black");
        // The same size passes through untouched.
        let same = bgra_to_i420(8, 8, &[0; 4].repeat(64), None).unwrap();
        assert_eq!(letterbox(same, (8, 8)).width(), 8);
    }
}
