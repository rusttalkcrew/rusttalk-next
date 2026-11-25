//! The camera: Settings → Video with a preview, and the call's camera.
//! Capture runs while the call has the camera on or the Video tab is open.

use std::{
    sync::{Arc, Mutex},
    thread,
};

use eframe::egui::{
    self, ColorImage, ComboBox, Image, Rect, RichText, TextureHandle, Ui, Vec2, accesskit::Role,
    pos2,
};
pub use rusttalk_core::camera::CameraOptions as Options;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use rusttalk_core::camera::{CameraCapture as Capture, list_cameras, start};
#[cfg(any(target_os = "android", target_os = "ios", test))]
mod mobile;
#[cfg(any(target_os = "android", target_os = "ios"))]
use mobile::{Capture, list_cameras, start};
use rusttalk_core::{
    camera::{FRAME_RATES, HEIGHTS},
    client::voice::NativeVideoSource,
};

use crate::{
    audio::{Settings, lock},
    call::{Listing, fill, upload},
    ui,
};

pub struct Camera {
    /// Turned on in the call.
    pub on: bool,
    /// `on` as of the last update, to retry a failed camera when it is turned on again.
    was_on: bool,
    /// The Video tab was open at the last update.
    preview: bool,
    /// Camera names for the picker, listed when the Video tab opens.
    names: Option<Arc<Mutex<Listing<String>>>>,
    running: Option<Running>,
    /// Finishes once the previous capture released the device; the next start waits for it.
    closing: Option<thread::JoinHandle<()>>,
    /// Which `Running` the call publishes.
    published: Option<u64>,
    texture: Option<TextureHandle>,
    /// Why the camera turned off by itself, to show once.
    pub problem: Option<String>,
    runtime: tokio::runtime::Handle,
    /// A phone camera in tests, which never starts real capture: what its menu offers,
    /// and what was picked.
    #[cfg(test)]
    pub fake: Option<Controls>,
    #[cfg(test)]
    pub test_picks: Vec<Pick>,
}

/// What the camera button's menu offers besides turning the camera off.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Controls {
    /// There is another camera to flip to.
    pub flip: bool,
    /// Whether the light is on, when the active camera has one.
    pub torch: Option<bool>,
    /// A flip or light change is still going on.
    pub busy: bool,
}

/// A choice from the camera button's menu.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pick {
    Flip,
    Torch(bool),
    Stop,
}

type Config = (Option<String>, Options);

struct Running {
    serial: u64,
    config: Config,
    /// Filled by the starting thread.
    capture: Arc<Mutex<Option<Result<Capture, String>>>>,
    starter: thread::JoinHandle<()>,
    #[cfg(any(target_os = "android", target_os = "ios"))]
    cancelled: Arc<std::sync::atomic::AtomicBool>,
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
            was_on: false,
            preview: false,
            names: None,
            running: None,
            closing: None,
            published: None,
            texture: None,
            problem: None,
            #[cfg(test)]
            fake: None,
            #[cfg(test)]
            test_picks: Vec::new(),
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
        settings: &mut Settings,
        in_call: bool,
        preview: bool,
    ) -> Option<Change> {
        self.on &= in_call;
        #[cfg(test)]
        if self.fake.is_some() {
            return None;
        }
        // On phones only an explicit call action starts capture.
        let preview = preview && !cfg!(any(target_os = "android", target_os = "ios"));
        self.preview = preview;
        #[cfg(any(target_os = "android", target_os = "ios"))]
        if self.on && self.running.is_none() {
            let permission = mobile::permission(!self.was_on);
            self.was_on = true;
            if permission == 0 {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return None;
            }
            if permission < 0 {
                self.problem = Some(t!("camera.permission").into());
                self.on = false;
            }
        }
        // A failure from before (e.g. in the Video tab) must not block turning the camera on.
        let retry =
            self.on && !self.was_on && matches!(self.ready().as_deref(), Some(Some(Err(_))));
        self.was_on = self.on;
        #[cfg(any(target_os = "android", target_os = "ios"))]
        if self.on {
            if let Some(running) = &mut self.running {
                if let Some(Ok(capture)) = lock(&running.capture).as_mut() {
                    if !capture.busy()
                        && running.config.0 == settings.camera
                        && self.published != Some(running.serial)
                    {
                        settings.camera = Some(capture.name().into());
                        running.config.0 = settings.camera.clone();
                    }
                    if let Some(ok) = capture.poll() {
                        if !ok {
                            self.problem = Some(t!("camera.switch_failed").into());
                        }
                        settings.camera = Some(capture.name().into());
                        running.config.0 = settings.camera.clone();
                        self.texture = None;
                    }
                    if capture.busy() {
                        settings.camera = running.config.0.clone();
                    }
                    if !capture.busy() && running.config.0 != settings.camera {
                        capture.select(settings.camera.as_deref());
                        running.config.0 = settings.camera.clone();
                    }
                }
            }
        }
        let wanted = (self.on || preview).then(|| (settings.camera.clone(), options(settings)));
        if retry || self.running.as_ref().map(|r| &r.config) != wanted.as_ref() {
            self.stop_now();
            self.running =
                wanted.map(|config| launch(ctx, &self.runtime, config, self.closing.take()));
        }
        let mut failed = None;
        if let Some(mut slot) = self.ready() {
            match &*slot {
                // Reported on the next frame by the arm below.
                Some(Ok(capture)) if capture.is_finished() => {
                    *slot = Some(Err(t!("camera.stopped").into()));
                    ctx.request_repaint();
                }
                Some(Err(error)) if self.on => failed = Some(error.clone()),
                Some(Ok(_)) => ctx.request_repaint_after(std::time::Duration::from_millis(33)),
                _ => {}
            }
        }
        if let Some(text) = failed {
            self.on = false;
            self.was_on = false;
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

    /// The call ended or dropped the camera; capture stops now unless the Video tab is open.
    pub fn reset(&mut self) {
        self.on = false;
        self.published = None;
        if !self.preview {
            self.stop_now();
        }
    }

    /// Stop capture and keep a handle that finishes once the device is free.
    fn stop_now(&mut self) {
        self.texture = None;
        let Some(running) = self.running.take() else {
            return;
        };
        #[cfg(any(target_os = "android", target_os = "ios"))]
        running
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        let taken = lock(&running.capture).take();
        let release = match taken {
            Some(Ok(capture)) => capture.stop(),
            Some(Err(_)) => return,
            // Still starting: stop it as soon as it has started.
            None => thread::spawn(move || {
                let _ = running.starter.join();
                if let Some(Ok(capture)) = lock(&running.capture).take() {
                    let _ = capture.stop().join();
                }
            }),
        };
        // `launch` took any earlier handle, so this one is the only one.
        self.closing = Some(release);
    }

    /// The mirrored self view, if the camera is running.
    pub fn self_view(&mut self, ui: &Ui, width: f32) -> Option<Image<'static>> {
        let mirrored = self.mirrored();
        let texture = self.self_texture(ui.ctx())?;
        let size = texture.size_vec2();
        Some(
            Image::new((texture.id(), Vec2::new(width, width * size.y / size.x)))
                .uv(preview_uv(mirrored)),
        )
    }

    pub fn mirrored(&self) -> bool {
        #[cfg(any(target_os = "android", target_os = "ios"))]
        return matches!(self.ready().as_deref(), Some(Some(Ok(capture))) if capture.name() == "front");
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        true
    }

    /// What the camera button offers while the camera is on: a menu on phones, which
    /// can flip the camera or switch its light; `None` where tapping just turns it off.
    pub fn controls(&self) -> Option<Controls> {
        if !self.on {
            return None;
        }
        #[cfg(test)]
        let controls = self.fake;
        #[cfg(all(not(test), any(target_os = "android", target_os = "ios")))]
        let controls = Some(match self.ready().as_deref() {
            Some(Some(Ok(capture))) => Controls {
                flip: capture.multiple,
                torch: capture.torch(),
                busy: capture.busy(),
            },
            // Still starting: only turning it off makes sense yet.
            _ => Controls::default(),
        });
        #[cfg(all(not(test), not(any(target_os = "android", target_os = "ios"))))]
        let controls = None;
        controls
    }

    /// The fake camera's self view picture.
    #[cfg(test)]
    pub fn fake_picture(&mut self, texture: TextureHandle) {
        self.texture = Some(texture);
    }

    /// Carry out a choice from the camera button's menu.
    pub fn pick(&mut self, pick: Pick, settings: &mut Settings) {
        #[cfg(test)]
        self.test_picks.push(pick);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        if let Some(mut slot) = self.ready()
            && let Some(Ok(capture)) = slot.as_mut()
            && !capture.busy()
        {
            match pick {
                Pick::Flip => {
                    let other = if capture.name() == "front" {
                        "back"
                    } else {
                        "front"
                    };
                    settings.camera = Some(other.into());
                }
                Pick::Torch(on) => capture.set_torch(on),
                Pick::Stop => {}
            }
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let _ = settings;
        if pick == Pick::Stop {
            self.on = false;
        }
    }

    /// The newest self view picture, not yet mirrored, if the camera is running.
    pub fn self_texture(&mut self, ctx: &egui::Context) -> Option<&TextureHandle> {
        #[cfg(test)]
        if self.fake.is_some() {
            return self.texture.as_ref();
        }
        let fresh = match self.ready()?.as_ref()? {
            Ok(capture) => capture.take_preview(),
            Err(_) => return None,
        };
        if let Some(frame) = fresh {
            let size = [frame.width() as usize, frame.height() as usize];
            // Opaque pixels: premultiplied and straight alpha are the same bytes.
            let image = ColorImage::from_rgba_premultiplied(size, frame.as_raw());
            upload(ctx, &mut self.texture, "self view", image);
        }
        self.texture.as_ref()
    }
}

fn launch(
    ctx: &egui::Context,
    runtime: &tokio::runtime::Handle,
    config: Config,
    previous: Option<thread::JoinHandle<()>>,
) -> Running {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let capture = Arc::new(Mutex::new(None));
    let slot = capture.clone();
    let ctx = ctx.clone();
    let (name, options) = config.clone();
    let runtime = runtime.clone();
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let stop = cancelled.clone();
    let starter = thread::spawn(move || {
        // The previous capture may still hold the device.
        if let Some(previous) = previous {
            let _ = previous.join();
        }
        let _runtime = runtime.enter();
        let started = list_cameras().and_then(|cameras| {
            // A saved camera that is gone falls back to the first one.
            let at = cameras
                .iter()
                .position(|camera| name.as_ref() == Some(&camera.name))
                .unwrap_or(0);
            let device = cameras
                .into_iter()
                .nth(at)
                .ok_or_else(|| anyhow::anyhow!("no camera found"))?;
            #[cfg(any(target_os = "android", target_os = "ios"))]
            return start(device, options, &stop);
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            start(device, options)
        });
        *lock(&slot) = Some(started.map_err(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "starting camera failed");
            if format!("{error:#}").contains("no camera found") {
                t!("camera.none").to_owned()
            } else {
                t!("camera.start_failed").to_owned()
            }
        }));
        ctx.request_repaint();
    });
    Running {
        serial: SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        config,
        capture,
        starter,
        #[cfg(any(target_os = "android", target_os = "ios"))]
        cancelled,
    }
}

/// Settings → Video.
pub fn settings_tab(ui: &mut Ui, camera: &mut Camera, s: &mut Settings) {
    let names = camera.names.get_or_insert_with(|| {
        let names = Arc::new(Mutex::new(Listing::Loading));
        fill(ui.ctx(), &names, "cameras", || {
            Ok(list_cameras()?.into_iter().map(|c| c.name).collect())
        });
        names
    });
    let found = match &*lock(names) {
        Listing::Found(found) => found.clone(),
        _ => Vec::new(),
    };
    ui::page_section(ui, t!("camera.camera"));
    ui::setting_row(ui, t!("camera.camera"), "", |ui| {
        let combo = ComboBox::from_id_salt("camera")
            .width(crate::call::PICKER_WIDTH)
            .selected_text(
                s.camera
                    .as_deref()
                    .map(camera_label)
                    .unwrap_or_else(|| t!("camera.first").into()),
            )
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut s.camera, None, t!("camera.first"));
                for name in found {
                    ui.selectable_value(&mut s.camera, Some(name.clone()), camera_label(&name));
                }
            });
        combo
            .response
            .widget_info(|| egui::WidgetInfo::labeled(Role::ComboBox, true, t!("camera.camera")));
    });
    let offered = match camera.ready().as_deref() {
        Some(Some(Ok(capture))) => Some(capture.heights.clone()),
        _ => None,
    };
    ui::setting_row(ui, t!("call.quality"), t!("camera.quality_help"), |ui| {
        for height in HEIGHTS {
            let label = format!("{height}p");
            let usable = offered
                .as_ref()
                .is_none_or(|heights| heights.is_empty() || heights.contains(&height));
            ui.add_enabled_ui(usable, |ui| {
                ui.selectable_value(&mut s.camera_height, height, label)
                    .on_disabled_hover_text(t!("camera.not_offered"));
            });
        }
    });
    ui::setting_row(
        ui,
        t!("call.frame_rate"),
        t!("camera.frame_rate_help"),
        |ui| {
            for fps in FRAME_RATES {
                ui.selectable_value(&mut s.camera_fps, fps, fps.to_string());
            }
        },
    );
    #[cfg(any(target_os = "android", target_os = "ios"))]
    if !camera.on {
        return;
    }
    ui::page_section(ui, t!("camera.preview"));
    let mode = match camera.ready().as_deref() {
        None | Some(None) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(t!("camera.starting"));
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
    ui.label(
        RichText::new(t!(
            "camera.sends",
            width = width,
            height = height,
            fps = fps
        ))
        .weak(),
    );
}

fn camera_label(name: &str) -> String {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    match name {
        "front" => return t!("camera.front").into(),
        "back" => return t!("camera.back").into(),
        _ => {}
    }
    name.to_owned()
}

fn preview_uv(mirrored: bool) -> Rect {
    if mirrored {
        Rect::from_min_max(pos2(1.0, 0.0), pos2(0.0, 1.0))
    } else {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
    }
}

#[cfg(any(target_os = "android", target_os = "ios", test))]
fn mobile_state(flags: i32) -> (&'static str, Option<bool>) {
    let front = flags & 1 != 0;
    let rear = flags & 2 != 0 && !front;
    (
        if front { "front" } else { "back" },
        (rear && flags & 4 != 0).then_some(flags & 8 != 0),
    )
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn torch_requires_an_active_supported_rear_camera() {
        assert_eq!(mobile_state(0).1, None);
        assert_eq!(mobile_state(1 | 4 | 8), ("front", None));
        assert_eq!(mobile_state(2), ("back", None));
        assert_eq!(mobile_state(2 | 4), ("back", Some(false)));
        assert_eq!(mobile_state(2 | 4 | 8), ("back", Some(true)));
        assert_eq!(mobile_state(4 | 8).1, None);
    }

    #[test]
    fn mirror_only_changes_horizontal_coordinates() {
        for mirrored in [false, true] {
            let uv = preview_uv(mirrored);
            assert_eq!((uv.min.y, uv.max.y), (0.0, 1.0));
            assert_eq!(
                (uv.min.x, uv.max.x),
                if mirrored { (1.0, 0.0) } else { (0.0, 1.0) }
            );
        }
    }
}
