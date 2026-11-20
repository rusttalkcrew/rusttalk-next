//! The camera: Settings → Video with a preview, and the call's camera.
//! Capture runs while the call has the camera on or the Video tab is open.

use std::{
    sync::{Arc, Mutex},
    thread,
};

use eframe::egui::{
    self, ColorImage, ComboBox, Image, Rect, RichText, TextureHandle, TextureOptions, Ui, Vec2,
    pos2,
};
pub use rusttalk_core::camera::{CameraCapture as Capture, CameraOptions as Options};
use rusttalk_core::{
    camera::{FRAME_RATES, HEIGHTS, list_cameras, start},
    client::voice::NativeVideoSource,
};

use crate::{
    audio::{Settings, lock},
    call::Listing,
    home::section,
};

pub struct Camera {
    /// Turned on in the call.
    pub on: bool,
    /// Camera names for the picker, listed when the Video tab opens.
    names: Option<Arc<Mutex<Listing<String>>>>,
    running: Option<Running>,
    /// Which `Running` the call publishes.
    published: Option<u64>,
    texture: Option<TextureHandle>,
    /// Why the camera turned off by itself, to show once.
    pub problem: Option<String>,
    runtime: tokio::runtime::Handle,
}

type Config = (Option<String>, Options);

struct Running {
    serial: u64,
    config: Config,
    /// Filled by the starting thread.
    capture: Arc<Mutex<Option<Result<Capture, String>>>>,
}

/// What the call should do with the camera this frame.
pub enum Change {
    Publish(NativeVideoSource, Options),
    Stop,
}

pub fn options(settings: &Settings) -> Options {
    let options = Options {
        height: settings.camera_height,
        fps: settings.camera_fps,
    };
    if HEIGHTS.contains(&options.height) && FRAME_RATES.contains(&options.fps) {
        options
    } else {
        Options::default()
    }
}

impl Camera {
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        Self {
            runtime,
            on: false,
            names: None,
            running: None,
            published: None,
            texture: None,
            problem: None,
        }
    }

    fn ready(&self) -> Option<std::sync::MutexGuard<'_, Option<Result<Capture, String>>>> {
        self.running.as_ref().map(|running| lock(&running.capture))
    }

    /// Start, restart or stop capture to match what is wanted, and say what the call must do.
    /// `in_call` means the call can publish; `preview` means the Video tab is open.
    pub fn update(
        &mut self,
        ctx: &egui::Context,
        settings: &Settings,
        in_call: bool,
        preview: bool,
    ) -> Option<Change> {
        self.on &= in_call;
        let wanted = (self.on || preview).then(|| (settings.camera.clone(), options(settings)));
        if self.running.as_ref().map(|r| &r.config) != wanted.as_ref() {
            self.running = wanted.map(|config| launch(ctx, &self.runtime, config));
            self.texture = None;
        }
        let mut failed = None;
        if let Some(mut slot) = self.ready() {
            match &*slot {
                // Reported on the next frame by the arm below.
                Some(Ok(capture)) if capture.is_finished() => {
                    *slot = Some(Err("Your camera stopped.".into()));
                }
                Some(Err(error)) if self.on => failed = Some(error.clone()),
                Some(Ok(_)) => ctx.request_repaint_after(std::time::Duration::from_millis(33)),
                _ => {}
            }
        }
        if let Some(text) = failed {
            self.on = false;
            self.problem = Some(text);
        }
        let serial = self.running.as_ref().map(|r| r.serial);
        let publish = if self.on { serial } else { None };
        if publish == self.published {
            return None;
        }
        if publish.is_none() {
            self.published = None;
            return Some(Change::Stop);
        }
        let options = self.running.as_ref()?.config.1;
        let source = match &*self.ready()? {
            Some(Ok(capture)) => capture.source.clone(),
            _ => return None,
        };
        self.published = publish;
        Some(Change::Publish(source, options))
    }

    /// The call ended; capture stops unless the Video tab is open.
    pub fn reset(&mut self) {
        self.on = false;
        self.published = None;
    }

    /// The mirrored self view, if the camera is running.
    pub fn self_view(&mut self, ui: &Ui, width: f32) -> Option<Image<'static>> {
        let fresh = match self.ready()?.as_ref()? {
            Ok(capture) => capture.take_preview(),
            Err(_) => return None,
        };
        if let Some(frame) = fresh {
            let size = [frame.width() as usize, frame.height() as usize];
            // Opaque pixels: premultiplied and straight alpha are the same bytes.
            let image = ColorImage::from_rgba_premultiplied(size, frame.as_raw());
            match &mut self.texture {
                Some(texture) => texture.set(image, TextureOptions::LINEAR),
                None => {
                    self.texture = Some(ui.ctx().load_texture(
                        "self view",
                        image,
                        TextureOptions::LINEAR,
                    ));
                }
            }
        }
        let texture = self.texture.as_ref()?;
        let size = texture.size_vec2();
        Some(
            Image::new((texture.id(), Vec2::new(width, width * size.y / size.x)))
                .uv(Rect::from_min_max(pos2(1.0, 0.0), pos2(0.0, 1.0))),
        )
    }
}

fn launch(ctx: &egui::Context, runtime: &tokio::runtime::Handle, config: Config) -> Running {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let capture = Arc::new(Mutex::new(None));
    let slot = capture.clone();
    let ctx = ctx.clone();
    let (name, options) = config.clone();
    let runtime = runtime.clone();
    thread::spawn(move || {
        let _runtime = runtime.enter();
        let started = list_cameras().and_then(|cameras| {
            let device = cameras
                .into_iter()
                .find(|camera| name.as_ref().is_none_or(|name| camera.name == *name))
                .ok_or_else(|| anyhow::anyhow!("no camera found"))?;
            start(device, options)
        });
        *lock(&slot) = Some(started.map_err(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "starting camera failed");
            if format!("{error:#}").contains("no camera found") {
                "No camera found.".to_owned()
            } else {
                "Couldn't start your camera. Is another app using it?".to_owned()
            }
        }));
        ctx.request_repaint();
    });
    Running {
        serial: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        config,
        capture,
    }
}

/// Settings → Video.
pub fn settings_tab(ui: &mut Ui, camera: &mut Camera, s: &mut Settings) {
    let names = camera.names.get_or_insert_with(|| {
        let names = Arc::new(Mutex::new(Listing::Loading));
        let slot = names.clone();
        let ctx = ui.ctx().clone();
        thread::spawn(move || {
            *lock(&slot) = match list_cameras() {
                Ok(found) => Listing::Found(found.into_iter().map(|c| c.name).collect()),
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "listing cameras failed");
                    Listing::Failed
                }
            };
            ctx.request_repaint();
        });
        names
    });
    let found = match &*lock(names) {
        Listing::Found(found) => found.clone(),
        _ => Vec::new(),
    };
    ComboBox::from_label("Camera")
        .width(280.0)
        .selected_text(s.camera.as_deref().unwrap_or("First camera"))
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut s.camera, None, "First camera");
            for name in found {
                ui.selectable_value(&mut s.camera, Some(name.clone()), name);
            }
        });
    let offered = match camera.ready().as_deref() {
        Some(Some(Ok(capture))) => Some(capture.heights.clone()),
        _ => None,
    };
    section(ui, "Quality");
    ui.horizontal(|ui| {
        for height in HEIGHTS {
            let label = format!("{height}p");
            let usable = offered
                .as_ref()
                .is_none_or(|heights| heights.is_empty() || heights.contains(&height));
            ui.add_enabled_ui(usable, |ui| {
                ui.selectable_value(&mut s.camera_height, height, label)
                    .on_disabled_hover_text("Your camera doesn't offer this");
            });
        }
    });
    section(ui, "Frame rate");
    ui.horizontal(|ui| {
        for fps in FRAME_RATES {
            ui.selectable_value(&mut s.camera_fps, fps, fps.to_string());
        }
    });
    ui.add_space(8.0);
    let mode = match camera.ready().as_deref() {
        None | Some(None) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Starting your camera…");
            });
            return;
        }
        Some(Some(Err(text))) => {
            ui.weak(text.as_str());
            return;
        }
        Some(Some(Ok(capture))) => capture.mode,
    };
    if let Some(image) = camera.self_view(ui, 320.0) {
        ui.add(image);
    }
    let (width, height, fps) = mode;
    ui.label(RichText::new(format!("Your camera sends {width}×{height} at {fps} fps")).weak());
}
