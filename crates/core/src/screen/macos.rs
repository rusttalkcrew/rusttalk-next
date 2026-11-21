//! Screen and window capture on macOS through ScreenCaptureKit.
//!
//! Sources are listed like on Windows: every display, then every titled normal window of
//! other apps in the current Space. Frames arrive as BGRA pixel buffers on a private
//! dispatch queue and are copied into the shared watch channel without row padding.

use std::{
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::{
    AllocAnyThread, DefinedClass, define_class, msg_send,
    rc::Retained,
    runtime::{NSObject, NSObjectProtocol, ProtocolObject},
};
use objc2_core_graphics::{
    CGDisplayIsBuiltin, CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate,
    SCStreamOutput, SCStreamOutputType,
};
use tokio::sync::watch;

use super::{BgraFrame, ScreenOptions, ScreenSource, ScreenSourceKind, frame_bytes};

/// A display or window by its system ID. Plain numbers, so the network tasks can hold
/// them; the live objects are looked up again when capture starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    Display(u32),
    Window(u32),
}

const DENIED: &str = "screen recording is off for RustTalk; allow it in System Settings, \
                      Privacy & Security, Screen & System Audio Recording, then reopen RustTalk";

/// macOS asks once; after allowing, the app must restart before capture works.
fn ensure_access() -> Result<()> {
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }
    CGRequestScreenCaptureAccess();
    bail!(DENIED)
}

/// What can be captured now, from ScreenCaptureKit's completion handler.
fn shareable() -> Result<Retained<SCShareableContent>> {
    ensure_access()?;
    let (send, receive) = mpsc::sync_channel(1);
    let done = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = match unsafe { Retained::retain(content) } {
                Some(content) => Ok(content),
                None => Err(unsafe { Retained::retain(error) }
                    .map(|error| error.localizedDescription().to_string())
                    .unwrap_or_else(|| "no content".into())),
            };
            let _ = send.try_send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            // Windows in other Spaces or of hidden apps deliver no frames until shown.
            true, true, &done,
        );
    }
    receive
        .recv_timeout(Duration::from_secs(10))
        .context("timed out listing screens")?
        .map_err(|error| anyhow!("listing screens failed: {error}"))
}

pub fn list_sources() -> Result<Vec<ScreenSource>> {
    let content = shareable()?;
    let mut sources = Vec::new();
    let displays = unsafe { content.displays() };
    for (i, display) in displays.iter().enumerate() {
        let id = unsafe { display.displayID() };
        let (width, height) = unsafe { (display.width(), display.height()) };
        let name = if CGDisplayIsBuiltin(id) {
            "Built-in display".to_owned()
        } else {
            format!("Display {}", i + 1)
        };
        sources.push(ScreenSource {
            name: format!("{name} ({width}×{height})"),
            kind: ScreenSourceKind::Monitor,
            target: Target::Display(id),
        });
    }
    let own = std::process::id() as i32;
    for window in unsafe { content.windows() }.iter() {
        // Layer 0 holds normal app windows; menus, the Dock and overlays sit above it.
        if unsafe { window.windowLayer() } != 0 {
            continue;
        }
        let app = unsafe { window.owningApplication() };
        if app
            .as_ref()
            .is_some_and(|app| unsafe { app.processID() } == own)
        {
            continue;
        }
        let title = unsafe { window.title() }
            .map(|title| title.to_string())
            .unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let app = app
            .map(|app| unsafe { app.applicationName() }.to_string())
            .unwrap_or_default();
        let name = if app.is_empty() || app == title {
            title
        } else {
            format!("{app}: {title}")
        };
        sources.push(ScreenSource {
            name,
            kind: ScreenSourceKind::Window,
            target: Target::Window(unsafe { window.windowID() }),
        });
    }
    Ok(sources)
}

struct Ivars {
    output: watch::Sender<Option<BgraFrame>>,
    stopped: Arc<AtomicBool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Output has no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "RustTalkScreenOutput"]
    #[ivars = Ivars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            let ivars = self.ivars();
            if ivars.output.is_closed() {
                return;
            }
            // Idle and blank frames carry no picture; the last one stays on screen.
            match unsafe { copy_frame(sample) } {
                Ok(Some(frame)) => {
                    let _ = ivars.output.send(Some(frame));
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not read a macOS screen frame");
                    ivars.stopped.store(true, Relaxed);
                }
            }
        }
    }

    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _: &SCStream, error: &NSError) {
            tracing::warn!(error = %error.localizedDescription(), "macOS screen capture stopped");
            self.ivars().stopped.store(true, Relaxed);
        }
    }
);

impl Output {
    fn new(output: watch::Sender<Option<BgraFrame>>, stopped: Arc<AtomicBool>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { output, stopped });
        unsafe { msg_send![super(this), init] }
    }
}

/// The frame's pixels without row padding, or None for a frame without a picture.
unsafe fn copy_frame(sample: &CMSampleBuffer) -> Result<Option<BgraFrame>> {
    let Some(pixels) = (unsafe { sample.image_buffer() }) else {
        return Ok(None);
    };
    let width = u32::try_from(CVPixelBufferGetWidth(&pixels))?;
    let height = u32::try_from(CVPixelBufferGetHeight(&pixels))?;
    let len = frame_bytes(width, height)?;
    let row = width as usize * 4;
    let locked = unsafe { CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) };
    ensure!(locked == 0, "could not lock a screen frame ({locked})");
    let copied = (|| {
        let stride = CVPixelBufferGetBytesPerRow(&pixels);
        let base = NonNull::new(CVPixelBufferGetBaseAddress(&pixels).cast::<u8>())
            .context("screen frame has no pixels")?;
        ensure!(stride >= row, "screen frame rows are too short");
        // SAFETY: the buffer is locked and holds `height` rows of `stride` bytes.
        let data = unsafe { std::slice::from_raw_parts(base.as_ptr(), stride * height as usize) };
        let mut out = Vec::with_capacity(len);
        for line in data.chunks_exact(stride) {
            out.extend_from_slice(&line[..row]);
        }
        Ok(out)
    })();
    unsafe { CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) };
    Ok(Some(BgraFrame {
        width,
        height,
        pixels: copied?,
    }))
}

pub struct Backend {
    stream: Retained<SCStream>,
    // Kept alive while the stream delivers to it; the stream holds it weakly.
    _output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    stopped: Arc<AtomicBool>,
}

// SAFETY: ScreenCaptureKit streams may be stopped from any thread, and the output object
// only touches its thread-safe ivars.
unsafe impl Send for Backend {}

impl Backend {
    pub fn is_finished(&self) -> bool {
        self.stopped.load(Relaxed)
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if !self.stopped.swap(true, Relaxed) {
            unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
        }
    }
}

pub async fn open(
    source: ScreenSource,
    options: ScreenOptions,
    _: &mut Option<String>,
) -> Result<(String, watch::Receiver<Option<BgraFrame>>, Backend)> {
    let (output, frames) = watch::channel(None);
    let backend =
        tokio::task::spawn_blocking(move || start(source.target, options, output)).await??;
    Ok((source.name, frames, backend))
}

fn start(
    target: Target,
    options: ScreenOptions,
    output: watch::Sender<Option<BgraFrame>>,
) -> Result<Backend> {
    let content = shareable()?;
    let filter = match target {
        Target::Display(id) => {
            let display = unsafe { content.displays() }
                .iter()
                .find(|display| unsafe { display.displayID() } == id)
                .context("the display is gone")?;
            unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::new(),
                )
            }
        }
        Target::Window(id) => {
            let window = unsafe { content.windows() }
                .iter()
                .find(|window| unsafe { window.windowID() } == id)
                .context("the window is gone")?;
            unsafe {
                SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
            }
        }
    };
    // Capture at the source's full pixel size; `screen.rs` scales down to the cap.
    let info = unsafe { SCShareableContent::infoForFilter(&filter) };
    let rect = unsafe { info.contentRect() };
    let scale = f64::from(unsafe { info.pointPixelScale() });
    let width = (rect.size.width * scale).round().max(2.0) as usize;
    let height = (rect.size.height * scale).round().max(2.0) as usize;
    frame_bytes(width as u32, height as u32)?;

    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        config.setWidth(width);
        config.setHeight(height);
        config.setPixelFormat(kCVPixelFormatType_32BGRA);
        config.setMinimumFrameInterval(CMTime::new(1, options.fps as i32));
        config.setShowsCursor(options.cursor);
        config.setQueueDepth(5);
        // A resized window keeps its shape inside the first size instead of being cut.
        config.setScalesToFit(true);
        config.setCapturesAudio(false);
    }

    let stopped = Arc::new(AtomicBool::new(false));
    let sink = Output::new(output, stopped.clone());
    let delegate = ProtocolObject::from_ref(&*sink);
    let stream = unsafe {
        SCStream::initWithFilter_configuration_delegate(
            SCStream::alloc(),
            &filter,
            &config,
            Some(delegate),
        )
    };
    let queue = DispatchQueue::new("rusttalk.screen", None);
    unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            ProtocolObject::from_ref(&*sink),
            SCStreamOutputType::Screen,
            Some(&queue),
        )
    }
    .map_err(|error| anyhow!("{}", error.localizedDescription()))
    .context("attach screen output")?;

    let (send, receive) = mpsc::sync_channel(1);
    let started = RcBlock::new(move |error: *mut NSError| {
        let error = unsafe { Retained::retain(error) }
            .map(|error| error.localizedDescription().to_string());
        let _ = send.try_send(error);
    });
    unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
    let failed = receive
        .recv_timeout(Duration::from_secs(10))
        .context("timed out starting screen capture")?;
    if let Some(error) = failed {
        bail!("starting screen capture failed: {error}");
    }
    Ok(Backend {
        stream,
        _output: sink,
        _queue: queue,
        stopped,
    })
}
