use anyhow::{Result, ensure};
use tokio::sync::watch;
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl},
    monitor::Monitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window,
};

use super::{BgraFrame, ScreenOptions, ScreenSource, ScreenSourceKind, frame_bytes};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    Monitor(Monitor),
    Window(Window),
}

pub fn list_sources() -> Result<Vec<ScreenSource>> {
    let mut sources = Vec::new();
    for (i, monitor) in Monitor::enumerate()?.into_iter().enumerate() {
        sources.push(ScreenSource {
            name: monitor
                .name()
                .ok()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| format!("Monitor {}", i + 1)),
            kind: ScreenSourceKind::Monitor,
            target: Target::Monitor(monitor),
        });
    }
    for window in Window::enumerate()? {
        if let Ok(name) = window.title()
            && !name.trim().is_empty()
        {
            sources.push(ScreenSource {
                name,
                kind: ScreenSourceKind::Window,
                target: Target::Window(window),
            });
        }
    }
    Ok(sources)
}

pub fn focused_window() -> Result<Option<ScreenSource>> {
    let own = std::process::id();
    let other = |window: &Window| {
        window.is_valid()
            && window.process_id().is_ok_and(|id| id != own)
            && window.title().is_ok_and(|title| !title.trim().is_empty())
    };
    let window = match Window::foreground() {
        // Pressed while RustTalk is in front: the window under it. EnumWindows goes
        // front to back.
        Ok(window) if window.process_id().is_ok_and(|id| id == own) => {
            Window::enumerate()?.into_iter().find(|w| other(w))
        }
        Ok(window) if other(&window) => Some(window),
        // The lock screen, a UAC prompt or the taskbar: never guess another window.
        _ => None,
    };
    Ok(window.map(|window| ScreenSource {
        name: window.title().unwrap_or_default(),
        kind: ScreenSourceKind::Window,
        target: Target::Window(window),
    }))
}

struct Handler {
    output: watch::Sender<Option<BgraFrame>>,
    padding: Vec<u8>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = watch::Sender<Option<BgraFrame>>;
    type Error = anyhow::Error;

    fn new(ctx: Context<Self::Flags>) -> Result<Self> {
        Ok(Self {
            output: ctx.flags,
            padding: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<()> {
        if self.output.is_closed() {
            control.stop();
            return Ok(());
        }
        let (width, height) = (frame.width(), frame.height());
        let len = frame_bytes(width, height)?;
        let buffer = frame.buffer()?;
        let pixels = buffer.as_nopadding_buffer(&mut self.padding);
        ensure!(pixels.len() == len, "invalid Windows capture frame length");
        let _ = self.output.send(Some(BgraFrame {
            width,
            height,
            pixels: pixels.to_vec(),
        }));
        Ok(())
    }
}

pub struct Backend(Option<CaptureControl<Handler, anyhow::Error>>);

impl Backend {
    pub fn is_finished(&self) -> bool {
        self.0
            .as_ref()
            .expect("active Windows capture")
            .is_finished()
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            std::thread::spawn(move || {
                let result = if control.is_finished() {
                    control.wait()
                } else {
                    control.stop()
                };
                if let Err(error) = result {
                    tracing::warn!(%error, "Windows screen capture ended");
                }
            });
        }
    }
}

pub async fn open(
    source: ScreenSource,
    options: ScreenOptions,
    _: &mut Option<String>,
) -> Result<(String, watch::Receiver<Option<BgraFrame>>, Backend)> {
    let (output, frames) = watch::channel(None);
    let backend = tokio::task::spawn_blocking(move || {
        let cursor = if options.cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        };
        // A little under the period, so a display refreshing at exactly the chosen rate
        // is not thinned by timing jitter; the feeder keeps frames to the rate.
        let interval = if GraphicsCaptureApi::is_minimum_update_interval_supported()? {
            MinimumUpdateIntervalSettings::Custom(options.period() * 3 / 4)
        } else {
            MinimumUpdateIntervalSettings::Default
        };
        let control = match source.target {
            Target::Monitor(monitor) => start(monitor, cursor, interval, output),
            Target::Window(window) => start(window, cursor, interval, output),
        }?;
        Ok::<_, anyhow::Error>(Backend(Some(control)))
    })
    .await??;
    Ok((source.name, frames, backend))
}

fn start(
    item: impl TryInto<GraphicsCaptureItemType> + Send + 'static,
    cursor: CursorCaptureSettings,
    interval: MinimumUpdateIntervalSettings,
    output: watch::Sender<Option<BgraFrame>>,
) -> Result<CaptureControl<Handler, anyhow::Error>> {
    Ok(Handler::start_free_threaded(Settings::new(
        item,
        cursor,
        DrawBorderSettings::Default,
        SecondaryWindowSettings::Default,
        interval,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        output,
    ))?)
}
