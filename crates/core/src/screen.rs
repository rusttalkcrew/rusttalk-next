//! Screen selection, capture and pixel conversion for native video tracks.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use livekit::webrtc::{
    native::yuv_helper::{abgr_to_i420, argb_to_i420, i420_to_abgr},
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
}
