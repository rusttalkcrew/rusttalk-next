//! Screen selection, capture and pixel conversion for native video tracks.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use livekit::webrtc::{
    native::yuv_helper::{argb_to_i420, i420_to_abgr},
    video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation},
    video_source::{VideoResolution, native::NativeVideoSource},
};
use tokio::{
    task::JoinHandle,
    time::{MissedTickBehavior, interval, timeout},
};

#[cfg(target_os = "linux")]
#[path = "screen/linux.rs"]
mod platform;
#[cfg(windows)]
#[path = "screen/windows.rs"]
mod platform;

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
    #[cfg(windows)]
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

/// Windows enumerates the interactive user's monitors and windows.
/// Linux offers one entry that opens the desktop's picker.
pub fn list_sources() -> Result<Vec<ScreenSource>> {
    #[cfg(any(target_os = "linux", windows))]
    return platform::list_sources();
    #[cfg(not(any(target_os = "linux", windows)))]
    anyhow::bail!("screen capture is unavailable on this platform")
}

struct BgraFrame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

fn frame_bytes(width: u32, height: u32) -> Result<usize> {
    ensure!(
        (1..=8192).contains(&width)
            && (1..=8192).contains(&height)
            && width * height <= 7680 * 4320,
        "screen frame dimensions out of range"
    );
    Ok((width * height * 4) as usize)
}

/// Convert tightly packed BGRA/BGRx to I420 and optionally scale down.
pub fn bgra_to_i420(
    width: u32,
    height: u32,
    pixels: &[u8],
    max_height: Option<u32>,
) -> Result<I420Buffer> {
    ensure!(
        pixels.len() == frame_bytes(width, height)?,
        "invalid BGRA frame length"
    );
    let mut frame = I420Buffer::new(width, height);
    let (ys, us, vs) = frame.strides();
    let (y, u, v) = frame.data_mut();
    argb_to_i420(
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
    if let Some(cap) = max_height.filter(|cap| *cap < height) {
        ensure!(cap > 0, "invalid screen resolution cap");
        let scaled_width = (width * cap / height).max(1);
        frame = frame.scale(scaled_width as i32, cap as i32);
    }
    Ok(frame)
}

/// Convert a decoded frame into tightly packed RGBA for display.
pub fn frame_to_rgba(frame: &dyn VideoBuffer) -> Result<Vec<u8>> {
    let (width, height) = (frame.width(), frame.height());
    let size = frame_bytes(width, height)?;
    let converted;
    let frame = if let Some(i420) = frame.as_i420() {
        i420
    } else {
        converted = frame.to_i420();
        &converted
    };
    let mut rgba = vec![0; size];
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
        &mut rgba,
        width * 4,
        width as i32,
        height as i32,
    );
    Ok(rgba)
}

/// A screen source owns capture until dropped. Dropping it requests shutdown.
pub struct ScreenCapture {
    pub name: String,
    pub source: NativeVideoSource,
    task: JoinHandle<()>,
}

impl ScreenCapture {
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
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
) -> Result<ScreenCapture> {
    #[cfg(any(target_os = "linux", windows))]
    {
        options.validate()?;
        let (name, mut frames, backend) = platform::open(source, options, restore_token).await?;
        timeout(Duration::from_secs(8), frames.changed())
            .await
            .context("timed out waiting for the first screen frame")?
            .context("screen capture ended before supplying video")?;
        let convert = move |frame: &BgraFrame| {
            bgra_to_i420(frame.width, frame.height, &frame.pixels, options.max_height)
        };
        let first = convert(
            frames
                .borrow_and_update()
                .as_ref()
                .context("missing first screen frame")?,
        )?;
        let source = NativeVideoSource::new(
            VideoResolution {
                width: first.width(),
                height: first.height(),
            },
            true,
        );
        // Seed the source before publication. Reuse converted pixels for static content.
        let mut frame = VideoFrame::new(VideoRotation::VideoRotation0, first);
        source.capture_frame(&frame);
        let feeder = source.clone();
        let task = tokio::spawn(async move {
            let mut tick = interval(options.period());
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if backend.is_finished() {
                    tracing::warn!("screen capture ended");
                    break;
                }
                match frames.has_changed() {
                    Ok(true) => {
                        let pending = frames.borrow_and_update();
                        if let Some(next) = pending.as_ref() {
                            match convert(next) {
                                Ok(next) => frame.buffer = next,
                                Err(error) => {
                                    tracing::warn!(%error, "could not convert captured screen frame");
                                    break;
                                }
                            }
                        }
                    }
                    Ok(false) => (),
                    Err(_) => break,
                }
                feeder.capture_frame(&frame);
            }
        });
        Ok(ScreenCapture { name, source, task })
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (source, options, restore_token);
        anyhow::bail!("screen capture is unavailable on this platform")
    }
}
