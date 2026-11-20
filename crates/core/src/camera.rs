//! Camera selection and capture for native video tracks (V4L2 on Linux, Media Foundation on
//! Windows). Frames are converted to I420 on a capture thread and fed to a LiveKit source.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

use crate::screen::rgba_to_i420;
use anyhow::{Context, Result, anyhow, ensure};
use livekit::webrtc::{
    video_frame::{I420Buffer, VideoBuffer, VideoFrame, VideoRotation},
    video_source::{VideoResolution, native::NativeVideoSource},
};
use nokhwa::{
    Buffer, Camera,
    pixel_format::RgbAFormat,
    utils::{
        ApiBackend, CameraFormat, CameraIndex, CameraInfo, FrameFormat, RequestedFormat,
        RequestedFormatType,
    },
};

/// Resolution choices offered to people. The camera's closest mode is used.
pub const HEIGHTS: [u32; 3] = [480, 720, 1080];
pub const FRAME_RATES: [u32; 2] = [15, 30];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraDevice {
    pub name: String,
    /// Device nodes with this name, lowest first. Linux also lists a USB webcam's
    /// metadata-only node under the same name; `start` tries each until one delivers video.
    indexes: Vec<CameraIndex>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CameraOptions {
    pub height: u32,
    pub fps: u32,
}

impl Default for CameraOptions {
    fn default() -> Self {
        Self {
            height: 720,
            fps: 30,
        }
    }
}

impl CameraOptions {
    pub(crate) fn validate(self) -> Result<()> {
        ensure!(HEIGHTS.contains(&self.height), "invalid camera resolution");
        ensure!(FRAME_RATES.contains(&self.fps), "invalid camera frame rate");
        Ok(())
    }
}

// Nokhwa initializes COM only on the first thread that touches Media Foundation.
#[cfg(windows)]
fn init_com() {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    // Same apartment as nokhwa's own call; S_FALSE on a thread that already has it.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

#[cfg(not(windows))]
fn init_com() {}

/// Cameras with their system names, one entry per name in device order. Blocks briefly;
/// call off the UI thread.
pub fn list_cameras() -> Result<Vec<CameraDevice>> {
    let found = std::thread::spawn(|| {
        init_com();
        nokhwa::query(ApiBackend::Auto)
    })
    .join()
    .map_err(|_| anyhow!("camera listing panicked"))??;
    Ok(group(found))
}

fn group(mut found: Vec<CameraInfo>) -> Vec<CameraDevice> {
    // V4L2 lists /dev/video* in directory order.
    found.sort_by_key(|info| info.index().as_index().unwrap_or(u32::MAX));
    let mut cameras: Vec<CameraDevice> = Vec::new();
    for info in found {
        let name = info.human_name();
        match cameras.iter_mut().find(|camera| camera.name == name) {
            Some(camera) => camera.indexes.push(info.index().clone()),
            None => cameras.push(CameraDevice {
                name,
                indexes: vec![info.index().clone()],
            }),
        }
    }
    cameras
}

/// The closest mode in height, then one that reaches the frame rate, then the one closest
/// to 16:9. Uncompressed wins ties.
fn pick(formats: &[CameraFormat], options: CameraOptions) -> Option<CameraFormat> {
    formats.iter().copied().min_by_key(|format| {
        let resolution = format.resolution();
        (
            resolution.height_y.abs_diff(options.height),
            options.fps.saturating_sub(format.frame_rate()),
            format.frame_rate().abs_diff(options.fps),
            resolution.width_x.abs_diff(resolution.height_y * 16 / 9),
            format.format() == FrameFormat::MJPEG,
        )
    })
}

pub type RgbaImage = image::RgbaImage;

fn to_i420(frame: &Buffer) -> Result<(I420Buffer, RgbaImage)> {
    let rgba = match frame.source_frame_format() {
        // Pure-Rust JPEG; nokhwa's own MJPEG decoder needs a C library.
        FrameFormat::MJPEG => {
            image::load_from_memory_with_format(frame.buffer(), image::ImageFormat::Jpeg)?
                .into_rgba8()
        }
        _ => frame.decode_image::<RgbAFormat>()?,
    };
    Ok((
        rgba_to_i420(rgba.width(), rgba.height(), rgba.as_raw())?,
        rgba,
    ))
}

/// A running camera. Dropping it stops capture and releases the device.
pub struct CameraCapture {
    pub source: NativeVideoSource,
    /// Width, height and frame rate the camera delivers.
    pub mode: (u32, u32, u32),
    /// Entries of `HEIGHTS` this camera offers.
    pub heights: Vec<u32>,
    preview: Arc<Mutex<Option<RgbaImage>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CameraCapture {
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stop capture without waiting. The handle finishes once the device is released;
    /// join it before opening the camera again.
    pub fn stop(mut self) -> JoinHandle<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().expect("capture thread")
    }

    /// The newest frame since the last call, for a local preview.
    pub fn take_preview(&self) -> Option<RgbaImage> {
        self.preview.lock().ok()?.take()
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        // The thread exits after its current frame; joining would block the caller.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// First frame size, delivered mode, and offered `HEIGHTS`.
type Opened = (VideoResolution, (u32, u32, u32), Vec<u32>);

/// Open a camera in the closest supported mode and wait for its first frame. Blocks for up
/// to 8 seconds: call off the UI thread, inside a Tokio runtime context (LiveKit's source
/// needs one).
pub fn start(device: CameraDevice, options: CameraOptions) -> Result<CameraCapture> {
    options.validate()?;
    let (ready, opened) = mpsc::sync_channel::<Result<Opened>>(1);
    let (give, take) = mpsc::sync_channel::<NativeVideoSource>(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let preview = Arc::new(Mutex::new(None));
    let latest = preview.clone();
    let thread = std::thread::Builder::new()
        .name("camera".into())
        .spawn(move || {
            init_com();
            let open = |index: CameraIndex| -> Result<(Camera, I420Buffer, Opened)> {
                let mut camera = Camera::new(
                    index,
                    RequestedFormat::new::<RgbAFormat>(RequestedFormatType::None),
                )?;
                let formats = camera.compatible_camera_formats()?;
                let format = pick(&formats, options).context("camera offers no usable mode")?;
                camera.set_camera_requset(RequestedFormat::with_formats(
                    RequestedFormatType::Exact(format),
                    &[format.format()],
                ))?;
                camera.open_stream()?;
                let (first, rgba) = to_i420(&camera.frame()?)?;
                *latest.lock().expect("preview lock") = Some(rgba);
                let size = VideoResolution {
                    width: first.width(),
                    height: first.height(),
                };
                let format = camera.camera_format();
                let heights = HEIGHTS
                    .into_iter()
                    .filter(|h| formats.iter().any(|f| f.resolution().height_y == *h))
                    .collect();
                let mode = (format.width(), format.height(), format.frame_rate());
                Ok((camera, first, (size, mode, heights)))
            };
            let mut opened = Err(anyhow!("camera has no device node"));
            for index in device.indexes {
                if stopped.load(Ordering::Relaxed) {
                    return;
                }
                opened = open(index);
                match &opened {
                    Ok(_) => break,
                    Err(error) => {
                        tracing::debug!(error = %format_args!("{error:#}"), "camera node unusable")
                    }
                }
            }
            let (mut camera, first, opened) = match opened {
                Ok(opened) => opened,
                Err(error) => {
                    let _ = ready.send(Err(error));
                    return;
                }
            };
            if ready.send(Ok(opened)).is_err() {
                return;
            }
            let Ok(source) = take.recv() else {
                return;
            };
            source.capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, first));
            let mut failures = 0;
            while !stopped.load(Ordering::Relaxed) {
                match camera
                    .frame()
                    .map_err(anyhow::Error::from)
                    .and_then(|f| to_i420(&f))
                {
                    Ok((frame, rgba)) => {
                        failures = 0;
                        source
                            .capture_frame(&VideoFrame::new(VideoRotation::VideoRotation0, frame));
                        if let Ok(mut slot) = latest.lock() {
                            *slot = Some(rgba);
                        }
                    }
                    // Webcams send the odd corrupt frame; only a run of them ends capture.
                    Err(error) if failures < 30 => {
                        failures += 1;
                        tracing::debug!(%error, "skipped camera frame");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "camera capture ended");
                        break;
                    }
                }
            }
            if let Err(error) = camera.stop_stream() {
                tracing::warn!(%error, "could not stop camera");
            }
        })?;
    let (size, mode, heights) = match opened.recv_timeout(Duration::from_secs(8)) {
        Ok(opened) => opened?,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            anyhow::bail!("camera thread ended before supplying video")
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            stop.store(true, Ordering::Relaxed);
            anyhow::bail!("timed out waiting for the first camera frame")
        }
    };
    let source = NativeVideoSource::new(size, false);
    give.send(source.clone())?;
    Ok(CameraCapture {
        source,
        mode,
        heights,
        preview,
        stop,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use nokhwa::utils::Resolution;

    use super::*;

    #[test]
    fn groups_device_nodes_by_name() {
        let node = |name, index| CameraInfo::new(name, "", "", CameraIndex::Index(index));
        let cameras = group(vec![node("Cam", 3), node("Other", 2), node("Cam", 0)]);
        let listed: Vec<_> = cameras
            .iter()
            .map(|camera| (camera.name.as_str(), camera.indexes.clone()))
            .collect();
        let index = CameraIndex::Index;
        assert_eq!(
            listed,
            [("Cam", vec![index(0), index(3)]), ("Other", vec![index(2)])]
        );
    }

    #[test]
    fn picks_the_closest_mode() {
        let mode = |w, h, format, fps| CameraFormat::new(Resolution::new(w, h), format, fps);
        let formats = [
            mode(640, 480, FrameFormat::YUYV, 30),
            mode(1280, 720, FrameFormat::YUYV, 10),
            mode(1280, 720, FrameFormat::MJPEG, 30),
            mode(1920, 1080, FrameFormat::MJPEG, 30),
            mode(1440, 1080, FrameFormat::YUYV, 30),
        ];
        let want = |height, fps| pick(&formats, CameraOptions { height, fps }).unwrap();
        assert_eq!(want(720, 30), formats[2]);
        assert_eq!(want(720, 15), formats[2]);
        assert_eq!(want(480, 15), formats[0]);
        assert_eq!(want(1080, 15), formats[3]);
        let wide = [formats[4], formats[3]];
        assert_eq!(
            pick(
                &wide,
                CameraOptions {
                    height: 1080,
                    fps: 30
                }
            ),
            Some(formats[3])
        );
        let slow = [formats[1]];
        assert_eq!(pick(&slow, CameraOptions::default()), Some(formats[1]));
        assert_eq!(pick(&[], CameraOptions::default()), None);
        assert!(
            CameraOptions {
                height: 360,
                fps: 30
            }
            .validate()
            .is_err()
        );
        assert!(
            CameraOptions {
                height: 720,
                fps: 60
            }
            .validate()
            .is_err()
        );
    }
}
