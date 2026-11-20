//! Voice in the window: who is in each room, the call bar, the room view,
//! the connection panel and Settings → Voice.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, Button, Color32, ColorImage, ComboBox, Event, Frame, Grid, Image, Key, Layout,
    Margin, Modal, Popup, PopupCloseBehavior, RichText, Role, Sense, Slider, Stroke, TextureHandle,
    TextureOptions, Ui, Vec2, Window,
};

use crate::{
    audio::{self, Audio, Settings, StreamId},
    camera::{Camera, Change},
    home::{Home, Tab, avatar, palette, section, toast_line},
    net::Command,
    screen::{self, Kind, Options, Target, Video},
    server::{Call, CallState, Id, Screen, Server, Stats},
    share::{self, App},
};

const GREEN: Color32 = Color32::from_rgb(0x23, 0xa5, 0x5a);
const AMBER: Color32 = Color32::from_rgb(0xf0, 0xb2, 0x32);
const RED: Color32 = Color32::from_rgb(0xda, 0x37, 0x3c);
/// Meter range in dBFS.
const FLOOR: f32 = -70.0;
const CEILING: f32 = -10.0;
const METER_WIDTH: f32 = 360.0;
/// Width of a person's tile while it shows their camera.
const VIDEO_WIDTH: f32 = 240.0;

pub struct Voice {
    pub audio: Audio,
    pub call: Option<Call>,
    pub show_stats: bool,
    /// Apps this device shares in the call.
    pub shares: Vec<App>,
    /// Apps playing sound when the picker opened, filled by a listing thread.
    apps: Arc<Mutex<Listing<App>>>,
    supported: bool,
    calibration: Option<Calibration>,
    picking_key: bool,
    pub video: Video,
    pub sharing: Sharing,
    /// The dialog's choices: options and the picked entry of `targets`.
    share_screen: Option<(Options, Option<usize>)>,
    /// Screens and windows to pick from (Windows), filled by a listing thread.
    targets: Arc<Mutex<Listing<Target>>>,
    pub watching: Option<Watching>,
    pub camera: Camera,
    /// People whose camera this device chose not to receive.
    hidden: HashSet<Id>,
    /// Camera tracks this device receives, with their newest picture.
    cameras: HashMap<String, Option<TextureHandle>>,
    /// Camera tracks drawn this frame; only these stay subscribed.
    shown: HashSet<String>,
}

pub enum Listing<T> {
    Loading,
    Failed,
    Found(Vec<T>),
}

#[derive(Clone, Copy, PartialEq)]
pub enum Sharing {
    Off,
    /// Waiting for the system picker or the first frame.
    Starting,
    On,
}

/// The screen shown large in the room view.
pub struct Watching {
    pub sid: String,
    /// Whose screen, for the title.
    title: String,
    texture: Option<TextureHandle>,
    /// Covers the whole window.
    pub full: bool,
}

enum Calibration {
    /// Until when.
    Quiet(Instant),
    Speak(Instant, Vec<f32>),
    Done(String),
}

impl Voice {
    pub fn new(audio: Audio, video: Video, runtime: tokio::runtime::Handle) -> Self {
        Self {
            audio,
            call: None,
            show_stats: false,
            shares: Vec::new(),
            apps: Arc::new(Mutex::new(Listing::Loading)),
            supported: share::supported(),
            calibration: None,
            picking_key: false,
            video,
            sharing: Sharing::Off,
            share_screen: None,
            targets: Arc::new(Mutex::new(Listing::Loading)),
            watching: None,
            camera: Camera::new(runtime),
            hidden: HashSet::new(),
            cameras: HashMap::new(),
            shown: HashSet::new(),
        }
    }

    /// The call ended or was left: stop sending audio and forget its shares and video.
    pub fn end_call(&mut self) {
        self.call = None;
        self.audio.set_voice(false);
        self.show_stats = false;
        self.shares.clear();
        self.clear_screens();
    }

    /// Forget this call's video: our screen and camera, and what we watch.
    pub fn clear_screens(&mut self) {
        self.sharing = Sharing::Off;
        self.share_screen = None;
        self.watching = None;
        self.camera.reset();
        self.cameras.clear();
    }

    fn in_call(&self, server: &Server, channel: &Id) -> bool {
        self.call
            .as_ref()
            .is_some_and(|call| call.server == server.address && call.channel == *channel)
    }

    fn speaking(&self, server: &Server, person: &Id) -> bool {
        if *person == server.dir.me {
            self.audio.talking()
        } else {
            self.audio.speaking(person)
        }
    }
}

/// Join a voice channel, leaving any other room first, and show it.
pub fn join(home: &mut Home, index: usize, channel: Id) {
    home.servers[index].open = Some(channel);
    if home.voice.in_call(&home.servers[index], &channel) {
        return;
    }
    leave(home);
    let server = &home.servers[index];
    server.send(Command::JoinVoice(channel));
    home.voice.call = Some(Call {
        server: server.address.clone(),
        channel,
        state: CallState::Connecting,
        stats: Stats::default(),
        can_talk: true,
        can_share_video: true,
    });
    home.voice.audio.set_voice(true);
}

pub fn leave(home: &mut Home) {
    let Some(call) = home.voice.call.take() else {
        return;
    };
    home.voice.end_call();
    if let Some(server) = home.servers.iter().find(|s| s.address == call.server) {
        server.send(Command::LeaveVoice);
    }
}

/// Once per frame: push-to-talk, the settings meter, and repaints for live indicators.
pub fn frame(ctx: &egui::Context, home: &mut Home) {
    let voice = &mut home.voice;
    let settings = voice.audio.settings();
    let key = Key::from_name(&settings.key);
    // shortcut: push-to-talk works only while RustTalk has focus; add a global hotkey
    // (per OS) if people ask to talk while another app is in front.
    let down = settings.push_to_talk
        && !voice.picking_key
        && !ctx.egui_wants_keyboard_input()
        && key.is_some_and(|key| ctx.input(|input| input.key_down(key)));
    voice.audio.set_key_down(down);
    let testing = home.settings == Some(Tab::Voice);
    voice.audio.set_testing(testing);
    if testing || voice.call.is_some() {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
    // Stop watching a screen once it is no longer shared.
    if let Some(watching) = &home.voice.watching {
        let shared = call_server(home)
            .zip(home.voice.call.as_ref())
            .and_then(|(server, call)| server.voice.get(&call.channel))
            .is_some_and(|peers| {
                peers
                    .iter()
                    .any(|peer| peer.screens.iter().any(|s| s.sid == watching.sid))
            });
        if !shared {
            stop_watching(home);
        }
    }
    let can_video = home
        .voice
        .call
        .as_ref()
        .is_some_and(|call| call.can_share_video);
    let preview = home.settings == Some(Tab::Video);
    let change = home.voice.camera.update(ctx, &settings, can_video, preview);
    if let Some(text) = home.voice.camera.problem.take() {
        home.toast = Some((text, ctx.input(|input| input.time) + 6.0));
    }
    // Field by field, so the camera state stays writable below.
    let Some(server) = (home.voice.call.as_ref())
        .and_then(|call| home.servers.iter().find(|s| s.address == call.server))
    else {
        return;
    };
    match change {
        Some(Change::Publish(source, options)) => {
            server.send(Command::ShareCamera(source, options))
        }
        Some(Change::Stop) => server.send(Command::StopCamera),
        None => {}
    }
    // Receive the cameras drawn last frame, and only those.
    let shown = std::mem::take(&mut home.voice.shown);
    for sid in &shown {
        if !home.voice.cameras.contains_key(sid) {
            server.send(Command::Watch(sid.clone(), true));
        }
    }
    home.voice.cameras.retain(|sid, _| {
        let keep = shown.contains(sid);
        if !keep {
            server.send(Command::Watch(sid.clone(), false));
        }
        keep
    });
    for sid in shown {
        home.voice.cameras.entry(sid).or_default();
    }
}

fn call_server(home: &Home) -> Option<&Server> {
    let call = home.voice.call.as_ref()?;
    home.servers.iter().find(|s| s.address == call.server)
}

fn watch(home: &mut Home, screen: &Screen, title: String) {
    stop_watching(home);
    if let Some(server) = call_server(home) {
        server.send(Command::Watch(screen.sid.clone(), true));
        home.voice.watching = Some(Watching {
            sid: screen.sid.clone(),
            title,
            texture: None,
            full: false,
        });
    }
}

fn stop_watching(home: &mut Home) {
    if let Some(watching) = home.voice.watching.take()
        && let Some(server) = call_server(home)
    {
        server.send(Command::Watch(watching.sid, false));
    }
}

/// The people in a voice channel, under its name in the channel list.
pub fn room_list(ui: &mut Ui, home: &Home, server: &Server, channel: &Id) {
    let Some(peers) = server.voice.get(channel) else {
        return;
    };
    for peer in peers {
        let name = peer.name(server);
        let row = ui.horizontal(|ui| {
            ui.add_space(22.0);
            let face = avatar(ui, &peer.person, &name, 22.0);
            if home.voice.speaking(server, &peer.person) {
                ring(ui, face.rect);
            }
            ui.label(RichText::new(&name).small());
            if peer.connecting {
                ui.label(RichText::new("Connecting…").small().weak());
            }
            if !peer.screens.is_empty() {
                live_badge(ui);
            }
        });
        let row = row.response.interact(Sense::click());
        let listening =
            peer.person != server.dir.me && !peer.unknown && home.voice.in_call(server, channel);
        if listening {
            Popup::context_menu(&row)
                .show(|ui| stream_menu(ui, &home.voice.audio, &(peer.person, None)));
        }
        for (sid, app) in &peer.apps {
            let stream = (peer.person, Some(sid.clone()));
            let row = ui.horizontal(|ui| {
                ui.add_space(52.0);
                ui.label(RichText::new(format!("🎵 {app}")).small());
                if listening && home.voice.audio.volume(&stream).1 {
                    ui.label(RichText::new("Muted").small().weak());
                }
            });
            if listening {
                let row = row.response.interact(Sense::click());
                Popup::context_menu(&row).show(|ui| stream_menu(ui, &home.voice.audio, &stream));
            }
        }
    }
}

fn ring(ui: &Ui, rect: egui::Rect) {
    ui.painter().circle_stroke(
        rect.center(),
        rect.width() / 2.0 + 2.0,
        Stroke::new(2.0, GREEN),
    );
}

/// Volume and mute for a person's voice or one app they share, in its context menu.
pub fn stream_menu(ui: &mut Ui, audio: &Audio, stream: &StreamId) {
    let (mut volume, mut muted) = audio.volume(stream);
    let before = (volume, muted);
    ui.label("Volume");
    volume_slider(ui, &mut volume);
    ui.checkbox(&mut muted, "Mute");
    if (volume, muted) != before {
        audio.set_volume(stream.clone(), volume, muted);
    }
}

fn volume_slider(ui: &mut Ui, volume: &mut f32) -> egui::Response {
    ui.add(
        Slider::new(volume, 0.0..=2.0)
            .custom_formatter(|value, _| format!("{:.0}%", value * 100.0))
            .custom_parser(|text| {
                let percent: f64 = text.trim().trim_end_matches('%').parse().ok()?;
                Some(percent / 100.0)
            }),
    )
}

/// The bar at the bottom of the channel list while in a voice room.
pub fn call_bar(ui: &mut Ui, home: &mut Home) {
    let Some(call) = &home.voice.call else {
        return;
    };
    let server = home.servers.iter().find(|s| s.address == call.server);
    let place = server
        .and_then(|server| {
            let (name, _) = server.chat_title(&call.channel)?;
            Some(format!("{name} / {}", server.name()))
        })
        .unwrap_or_default();
    let (text, color) = match call.state {
        CallState::Connecting => ("Connecting…", AMBER),
        CallState::Connected => ("Voice connected", GREEN),
        CallState::Reconnecting => ("Reconnecting…", AMBER),
    };
    let audio = home.voice.audio.clone();
    let connected = call.state == CallState::Connected;
    let can_talk = call.can_talk;
    let can_share_video = call.can_share_video;
    let mut leave_now = false;
    let mut share = None;
    let mut stop = None;
    let mut stop_screen = false;
    Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(6)
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).strong().color(color));
            ui.label(RichText::new(place).small().weak());
            let off = RED.gamma_multiply(0.6);
            let selected = ui.visuals().selection.bg_fill;
            ui.horizontal(|ui| {
                let deafened = audio.deafened();
                let muted = audio.muted() || deafened;
                let mic = if muted { "🔇" } else { "🎤" };
                if toggle(
                    ui,
                    mic,
                    if muted { "Unmute" } else { "Mute" },
                    muted.then_some(off),
                )
                .clicked()
                {
                    audio.set_muted(!muted);
                    if muted {
                        audio.set_deafened(false);
                    }
                }
                let ears = if deafened { "Undeafen" } else { "Deafen" };
                if toggle(ui, "🎧", ears, deafened.then_some(off)).clicked() {
                    audio.set_deafened(!deafened);
                }
                if toggle(
                    ui,
                    "📶",
                    "Connection",
                    home.voice.show_stats.then_some(selected),
                )
                .clicked()
                {
                    home.voice.show_stats ^= true;
                }
                if toggle(ui, "⚙", "Voice settings", None).clicked() {
                    home.settings = Some(Tab::Voice);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let hang = ui
                        .add(Button::new(RichText::new("📞").color(Color32::WHITE)).fill(RED))
                        .on_hover_text("Leave");
                    hang.widget_info(|| egui::WidgetInfo::labeled(Role::Button, true, "Leave"));
                    leave_now = hang.clicked();
                });
            });
            share_picker(
                ui,
                &mut home.voice,
                connected,
                can_talk,
                &mut share,
                &mut stop,
            );
            camera_button(ui, &mut home.voice.camera, connected, can_share_video);
            if screen::SUPPORTED {
                stop_screen = screen_button(ui, &mut home.voice, connected, can_share_video);
            }
            // W12 live transfer seam: "Send file" and the offered-file cards go here,
            // once VoiceRoom::offer_live_file is merged.
        });
    let server = home
        .voice
        .call
        .as_ref()
        .and_then(|call| home.servers.iter().find(|s| s.address == call.server));
    if let (Some(server), Some((app, on))) = (server, share) {
        if on {
            home.voice.shares.push(app.clone());
            server.send(Command::ShareApp(app));
        } else {
            home.voice.shares.retain(|shared| shared.id != app.id);
            server.send(Command::StopApp(app.id.clone()));
        }
    }
    if let (Some(server), Some(id)) = (server, stop) {
        home.voice.shares.retain(|shared| shared.id != id);
        server.send(Command::StopApp(id));
    }
    if let (Some(server), true) = (server, stop_screen) {
        home.voice.sharing = Sharing::Off;
        server.send(Command::StopScreen);
    }
    if leave_now {
        leave(home);
    }
}

/// Turns the camera on or off in the call.
fn camera_button(ui: &mut Ui, camera: &mut Camera, connected: bool, allowed: bool) {
    let text = if camera.on {
        "📷  Turn off camera"
    } else {
        "📷  Turn on camera"
    };
    let mut button = Button::new(text).min_size(Vec2::new(ui.available_width(), 28.0));
    if camera.on {
        button = button.fill(ui.visuals().selection.bg_fill);
    }
    let button = ui.add_enabled(connected && allowed, button);
    let button = if allowed {
        button
    } else {
        button.on_disabled_hover_text("You can't share video here")
    };
    if button.clicked() {
        camera.on ^= true;
    }
}

/// "Share screen", or while sharing a chip with Stop. True when Stop was clicked.
fn screen_button(ui: &mut Ui, voice: &mut Voice, connected: bool, allowed: bool) -> bool {
    let text = match voice.sharing {
        Sharing::Off => {
            let button =
                Button::new("🖥  Share screen").min_size(Vec2::new(ui.available_width(), 28.0));
            let button = if allowed {
                ui.add_enabled(connected, button)
            } else {
                ui.add_enabled(false, button)
                    .on_disabled_hover_text("You can't share your screen here")
            };
            if button.clicked() {
                voice.share_screen = Some((Options::default(), None));
                list_targets(ui.ctx(), voice);
            }
            return false;
        }
        Sharing::Starting => "Starting your screen…",
        Sharing::On => "You're sharing your screen",
    };
    let mut stop = false;
    Frame::new()
        .fill(GREEN.gamma_multiply(0.25))
        .corner_radius(4)
        .inner_margin(Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(text).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    stop = ui.small_button("Stop").clicked();
                });
            });
        });
    stop
}

fn list_targets(ctx: &egui::Context, voice: &Voice) {
    fill(ctx, &voice.targets, "screens", screen::targets);
}

/// List `what` on its own thread into `slot`, then repaint.
pub fn fill<T: Send + 'static>(
    ctx: &egui::Context,
    slot: &Arc<Mutex<Listing<T>>>,
    what: &'static str,
    list: impl FnOnce() -> anyhow::Result<Vec<T>> + Send + 'static,
) {
    *audio::lock(slot) = Listing::Loading;
    let slot = slot.clone();
    let ctx = ctx.clone();
    thread::spawn(move || {
        let found = match list() {
            Ok(found) => Listing::Found(found),
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "listing {what} failed");
                Listing::Failed
            }
        };
        *audio::lock(&slot) = found;
        ctx.request_repaint();
    });
}

/// Show `image` in the texture in `slot`, creating it on first use.
pub fn upload(
    ctx: &egui::Context,
    slot: &mut Option<TextureHandle>,
    name: &str,
    image: ColorImage,
) {
    match slot {
        Some(texture) => texture.set(image, TextureOptions::LINEAR),
        None => *slot = Some(ctx.load_texture(name, image, TextureOptions::LINEAR)),
    }
}

/// The "Share your screen" dialog: what to share (Windows), quality, frame rate, cursor.
pub fn screen_dialog(ctx: &egui::Context, home: &mut Home) {
    let voice = &mut home.voice;
    let Some((options, picked)) = &mut voice.share_screen else {
        return;
    };
    let mut close = false;
    let mut share = None;
    let modal = Modal::new(egui::Id::unique("share screen")).show(ctx, |ui| {
        ui.set_width(400.0);
        ui.heading("Share your screen");
        let targets = audio::lock(&voice.targets);
        // Linux lists one entry that opens the system's own picker.
        let portal = match &*targets {
            Listing::Loading => {
                ui.spinner();
                None
            }
            Listing::Failed => {
                ui.weak("Couldn't look for screens. Try again.");
                None
            }
            Listing::Found(found) => {
                let portal = found.iter().position(|t| t.kind == Kind::Portal);
                if portal.is_none() {
                    for (heading, kind) in [("Screens", Kind::Monitor), ("Windows", Kind::Window)] {
                        section(ui, heading);
                        egui::ScrollArea::vertical()
                            .id_salt(heading)
                            .max_height(160.0)
                            .show(ui, |ui| {
                                for (at, item) in found.iter().enumerate() {
                                    if item.kind == kind
                                        && ui
                                            .selectable_label(*picked == Some(at), &item.name)
                                            .clicked()
                                    {
                                        *picked = Some(at);
                                    }
                                }
                            });
                    }
                }
                portal
            }
        };
        section(ui, "Quality");
        ui.horizontal(|ui| {
            for (height, label) in [(Some(720), "720p"), (Some(1080), "1080p"), (None, "Native")] {
                ui.selectable_value(&mut options.max_height, height, label);
            }
        });
        section(ui, "Frame rate");
        ui.horizontal(|ui| {
            for fps in [15, 30, 60] {
                ui.selectable_value(&mut options.fps, fps, fps.to_string());
            }
        });
        ui.add_space(6.0);
        ui.checkbox(&mut options.cursor, "Show my cursor");
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let label = if portal.is_some() {
                "Choose what to share"
            } else {
                "Share"
            };
            let target = match &*targets {
                Listing::Found(found) => portal.or(*picked).and_then(|at| found.get(at)),
                _ => None,
            };
            let button = Button::new(label).fill(ui.visuals().selection.bg_fill);
            if ui.add_enabled(target.is_some(), button).clicked() {
                share = target.cloned();
            }
            close |= ui.button("Cancel").clicked();
        });
    });
    if let Some(target) = share {
        let command = Command::ShareScreen(target, *options);
        if let Some(server) = call_server(home) {
            server.send(command);
            home.voice.sharing = Sharing::Starting;
        }
        close = true;
    }
    if close || modal.should_close() {
        home.voice.share_screen = None;
    }
}

/// A red "LIVE" tag for someone sharing their screen.
fn live_badge(ui: &mut Ui) {
    ui.label(
        RichText::new(" LIVE ")
            .small()
            .strong()
            .color(Color32::WHITE)
            .background_color(RED),
    );
}

/// The watched screen: title, full window toggle, Stop watching, and the picture.
pub fn screen_view(ui: &mut Ui, home: &mut Home) {
    let voice = &mut home.voice;
    let Some(watching) = &mut voice.watching else {
        return;
    };
    // Upload only the newest converted frame.
    if let Some(image) = voice.video.take(&watching.sid) {
        upload(ui.ctx(), &mut watching.texture, "screen", image);
    }
    let mut stop = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(&watching.title).strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            stop = ui.button("Stop watching").clicked();
            let label = if watching.full {
                "Exit full window"
            } else {
                "Full window"
            };
            if ui.button(label).clicked() {
                watching.full ^= true;
            }
        });
    });
    if watching.full && ui.input(|input| input.key_pressed(Key::Escape)) {
        watching.full = false;
    }
    let height = if watching.full {
        ui.available_height()
    } else {
        ui.available_height() * 0.6
    };
    let area = Vec2::new(ui.available_width(), height.max(120.0));
    match &watching.texture {
        Some(texture) => {
            let size = texture.size_vec2();
            let scale = (area.x / size.x).min(area.y / size.y);
            ui.vertical_centered(|ui| ui.add(Image::new((texture.id(), size * scale))));
        }
        None => {
            ui.allocate_ui(area, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("Waiting for the picture…");
                    });
                });
            });
        }
    }
    if stop {
        stop_watching(home);
    }
}

/// "Share app audio", its app picker, and a chip with a stop button per shared app.
fn share_picker(
    ui: &mut Ui,
    voice: &mut Voice,
    connected: bool,
    can_talk: bool,
    share: &mut Option<(App, bool)>,
    stop: &mut Option<String>,
) {
    let button = Button::new("🎵  Share app audio").min_size(Vec2::new(ui.available_width(), 28.0));
    let button = if !voice.supported {
        ui.add_enabled(false, button)
            .on_disabled_hover_text("Needs Windows 11")
    } else if !can_talk {
        ui.add_enabled(false, button)
            .on_disabled_hover_text("You can't talk here")
    } else {
        ui.add_enabled(connected, button)
    };
    if button.clicked() {
        // Listing starts a helper process on Linux; keep it off the UI thread.
        fill(ui.ctx(), &voice.apps, "apps", share::list);
    }
    Popup::menu(&button)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| {
            ui.set_min_width(200.0);
            ui.label(RichText::new("Pick apps to share").strong());
            match &*audio::lock(&voice.apps) {
                Listing::Loading => {
                    ui.spinner();
                }
                Listing::Failed => {
                    ui.weak("Couldn't look for apps. Try again.");
                }
                Listing::Found(apps) if apps.is_empty() => {
                    ui.weak("No apps are playing sound.");
                }
                Listing::Found(apps) => {
                    for app in apps {
                        let was = voice.shares.iter().any(|shared| shared.id == app.id);
                        let mut on = was;
                        ui.checkbox(&mut on, &app.name);
                        if on != was {
                            *share = Some((app.clone(), on));
                        }
                    }
                }
            }
        });
    if voice.shares.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for app in &voice.shares {
            let chip = ui
                .add(Button::new(
                    RichText::new(format!("🎵 {}  ×", app.name)).small(),
                ))
                .on_hover_text("Stop sharing");
            chip.widget_info(|| {
                egui::WidgetInfo::labeled(Role::Button, true, format!("Stop sharing {}", app.name))
            });
            if chip.clicked() {
                *stop = Some(app.id.clone());
            }
        }
    });
}

/// An icon button; `on` fills it, with `fill` or the selection color.
fn toggle(ui: &mut Ui, icon: &str, label: &str, on: Option<Color32>) -> egui::Response {
    let mut button = Button::new(RichText::new(icon).size(16.0)).min_size(Vec2::splat(30.0));
    if let Some(fill) = on {
        button = button.fill(fill);
    }
    let response = ui.add(button).on_hover_text(label);
    response.widget_info(|| egui::WidgetInfo::labeled(Role::Button, true, label));
    response
}

/// The main pane for a voice channel: everyone in it, with speaking rings.
pub fn room(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    let server = &home.servers[index];
    let title = server
        .chat_title(&channel)
        .map(|(name, _)| name)
        .unwrap_or_default();
    let joined = home.voice.in_call(server, &channel);
    ui.label(RichText::new(format!("🔊 {title}")).strong().size(17.0));
    ui.separator();
    // Problems from the call bar and the room show here; the chat has its own line.
    toast_line(ui, &mut home.toast);
    let peers = server.voice.get(&channel).cloned().unwrap_or_default();
    if joined && home.voice.watching.is_some() {
        screen_view(ui, home);
        ui.add_space(8.0);
    }
    let server = &home.servers[index];
    let mut watch_now = None;
    if peers.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.weak(if joined {
                "Connecting…"
            } else {
                "No one is here yet."
            });
        });
    }
    let mut hide = None;
    ui.horizontal_wrapped(|ui| {
        for peer in &peers {
            let name = peer.name(server);
            let me = peer.person == server.dir.me;
            let picture = match &peer.camera {
                _ if !joined => None,
                Some(_) if me => home.voice.camera.self_view(ui, VIDEO_WIDTH),
                Some(sid) if !home.voice.hidden.contains(&peer.person) => {
                    home.voice.shown.insert(sid.clone());
                    camera_picture(ui, &mut home.voice, sid)
                }
                _ => None,
            };
            let tile = Frame::new()
                .fill(palette(ui).side)
                .corner_radius(8)
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(if picture.is_some() {
                        VIDEO_WIDTH
                    } else {
                        150.0
                    });
                    ui.vertical_centered(|ui| {
                        let video = picture.is_some();
                        let face = match picture {
                            Some(picture) => ui.add(picture),
                            None => avatar(ui, &peer.person, &name, 64.0),
                        };
                        if home.voice.speaking(server, &peer.person) {
                            if video {
                                ui.painter().rect_stroke(
                                    face.rect,
                                    4.0,
                                    Stroke::new(2.0, GREEN),
                                    egui::StrokeKind::Outside,
                                );
                            } else {
                                ring(ui, face.rect);
                            }
                        }
                        ui.label(RichText::new(&name).strong());
                        if !peer.screens.is_empty() {
                            live_badge(ui);
                        }
                        if peer.connecting {
                            ui.weak("Connecting…");
                        } else if me && home.voice.audio.deafened() {
                            ui.weak("Deafened");
                        } else if me && home.voice.audio.muted() {
                            ui.weak("Muted");
                        } else if !me && home.voice.audio.volume(&(peer.person, None)).1 {
                            ui.weak("Muted by you");
                        } else {
                            ui.label(" ");
                        }
                    });
                });
            let tile = tile.response.interact(Sense::click());
            // Click a live person's tile to watch their screen.
            if let Some(screen) = peer.screens.first().filter(|_| !me && joined) {
                let title = format!("{name}'s screen");
                let label = format!("Watch {title}");
                tile.widget_info(|| egui::WidgetInfo::labeled(Role::Button, true, &label));
                if tile.clone().on_hover_text("Watch").clicked() {
                    watch_now = Some((screen.clone(), title));
                }
            }
            if !me && !peer.unknown && joined {
                Popup::context_menu(&tile).show(|ui| {
                    stream_menu(ui, &home.voice.audio, &(peer.person, None));
                    if peer.camera.is_some() {
                        let mut show = !home.voice.hidden.contains(&peer.person);
                        if ui.checkbox(&mut show, "Show video").changed() {
                            hide = Some((peer.person, !show));
                        }
                    }
                });
            }
        }
    });
    match hide {
        Some((person, true)) => {
            home.voice.hidden.insert(person);
        }
        Some((person, false)) => {
            home.voice.hidden.remove(&person);
        }
        None => {}
    }
    if peers.iter().any(|peer| !peer.apps.is_empty()) || joined && !home.voice.shares.is_empty() {
        ui.add_space(12.0);
        section(ui, "Shared audio");
        let mut stop = None;
        Grid::new("shared audio")
            .num_columns(3)
            .spacing(Vec2::new(12.0, 6.0))
            .show(ui, |ui| {
                for peer in &peers {
                    if peer.person == server.dir.me {
                        continue;
                    }
                    let name = peer.name(server);
                    for (sid, app) in &peer.apps {
                        ui.label(format!("{name} · {app}"));
                        stream_row(
                            ui,
                            &home.voice.audio,
                            (peer.person, Some(sid.clone())),
                            &format!("{name} · {app}"),
                        );
                        ui.end_row();
                    }
                }
                if joined {
                    for app in &home.voice.shares {
                        ui.label(format!("You · {}", app.name));
                        if ui.button("Stop sharing").clicked() {
                            stop = Some(app.id.clone());
                        }
                        ui.label("");
                        ui.end_row();
                    }
                }
            });
        if let Some(id) = stop {
            home.voice.shares.retain(|shared| shared.id != id);
            home.servers[index].send(Command::StopApp(id));
        }
    }
    if let Some((screen, title)) = watch_now {
        watch(home, &screen, title);
    }
    if !joined {
        ui.add_space(12.0);
        if crate::app::primary(ui, "Join voice").clicked() {
            join(home, index, channel);
        }
    }
}

/// A watched camera's newest picture, once one arrived.
fn camera_picture(ui: &Ui, voice: &mut Voice, sid: &str) -> Option<Image<'static>> {
    let slot = voice.cameras.get_mut(sid)?;
    if let Some(image) = voice.video.take(sid) {
        upload(ui.ctx(), slot, sid, image);
    }
    let size = slot.as_ref()?.size_vec2();
    let texture = slot.as_ref()?.id();
    Some(Image::new((
        texture,
        Vec2::new(VIDEO_WIDTH, VIDEO_WIDTH * size.y / size.x),
    )))
}

/// Volume slider and mute button for one stream, in a grid row.
fn stream_row(ui: &mut Ui, audio: &Audio, stream: StreamId, name: &str) {
    let (mut volume, muted) = audio.volume(&stream);
    let slider = volume_slider(ui, &mut volume);
    slider.widget_info(|| egui::WidgetInfo::labeled(Role::Slider, true, format!("{name} volume")));
    let icon = if muted { "🔇" } else { "🔊" };
    let label = if muted { "Unmute" } else { "Mute" };
    let fill = muted.then_some(RED.gamma_multiply(0.6));
    let mute = toggle(ui, icon, &format!("{label} {name}"), fill);
    if slider.changed() || mute.clicked() {
        audio.set_volume(stream, volume, muted ^ mute.clicked());
    }
}

/// The connection panel, opened from the call bar.
pub fn stats_window(ctx: &egui::Context, home: &mut Home) {
    let Some(call) = &home.voice.call else {
        return;
    };
    let mut open = home.voice.show_stats;
    let server = home.servers.iter().find(|s| s.address == call.server);
    Window::new("Connection")
        .open(&mut open)
        .resizable(false)
        .collapsible(false)
        .default_width(240.0)
        // Above the call bar, beside the server rail.
        .pivot(egui::Align2::LEFT_BOTTOM)
        .default_pos(ctx.content_rect().left_bottom() + Vec2::new(84.0, -150.0))
        .show(ctx, |ui| {
            let stats = &call.stats;
            let ms = |value: Option<f64>| value.map_or("—".into(), |v| format!("{v:.0} ms"));
            let count = |value: Option<i64>| value.map_or("—".into(), |v| v.to_string());
            Grid::new("connection").num_columns(2).show(ui, |ui| {
                ui.label("Ping");
                ui.label(ms(stats.ping_ms));
                ui.end_row();
                ui.label("Jitter");
                ui.label(ms(stats.jitter_ms));
                ui.end_row();
                ui.label("Lost packets in");
                ui.label(count(stats.lost_in));
                ui.end_row();
                ui.label("Lost packets out");
                ui.label(count(stats.lost_out));
                ui.end_row();
            });
            section(ui, "Bitrate");
            if stats.tracks.is_empty() {
                ui.weak("No audio yet.");
            }
            Grid::new("bitrates").num_columns(2).show(ui, |ui| {
                for track in &stats.tracks {
                    let name = match (track.mine, server) {
                        (true, _) => "You".to_owned(),
                        (false, Some(server)) => server.member_name(&track.person),
                        (false, None) => String::new(),
                    };
                    match &track.app {
                        Some(app) => ui.label(format!("{name} · {app}")),
                        None => ui.label(name),
                    };
                    ui.label(format!("{:.0} kbps", track.kbps));
                    ui.end_row();
                }
            });
        });
    home.voice.show_stats &= open;
}

/// Settings → Voice.
pub fn settings_tab(ui: &mut Ui, voice: &mut Voice) {
    let audio = voice.audio.clone();
    let mut s = audio.settings();
    let (inputs, outputs) = audio.devices();
    device_picker(ui, "Microphone", &mut s.input, &inputs);
    device_picker(ui, "Speakers", &mut s.output, &outputs);
    if let Some(problem) = audio.problem() {
        ui.colored_label(ui.visuals().error_fg_color, problem);
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.selectable_value(&mut s.push_to_talk, false, "Voice activity");
        ui.selectable_value(&mut s.push_to_talk, true, "Push to talk");
    });
    meter(ui, &audio, &s);
    if s.push_to_talk {
        key_picker(ui, voice, &mut s);
    } else {
        // As wide as the meter, so the handle sits on the line.
        ui.spacing_mut().slider_width = METER_WIDTH;
        let slider = ui.add(Slider::new(&mut s.threshold, FLOOR..=CEILING).show_value(false));
        slider.widget_info(|| egui::WidgetInfo::labeled(Role::Slider, true, "Sensitivity"));
        ui.weak("Your mic turns on when the level passes the line. Drag to change it.");
        calibrate(ui, voice, &mut s);
    }
    ui.add_space(6.0);
    ui.checkbox(&mut s.echo_cancellation, "Echo cancellation");
    ui.checkbox(&mut s.noise_suppression, "Noise suppression");
    ui.checkbox(&mut s.auto_gain, "Automatic gain");
    audio.set_settings(s);
}

fn device_picker(
    ui: &mut Ui,
    label: &str,
    chosen: &mut Option<String>,
    devices: &[audio::DeviceInfo],
) {
    let current = match chosen {
        None => "System default",
        Some(id) => devices
            .iter()
            .find(|d| d.id == *id)
            .map_or("Not connected", |d| d.name.as_str()),
    };
    ComboBox::from_label(label)
        .width(280.0)
        .selected_text(current)
        .show_ui(ui, |ui| {
            ui.selectable_value(chosen, None, "System default");
            for device in devices {
                ui.selectable_value(chosen, Some(device.id.clone()), &device.name);
            }
        });
}

/// The live input level, with the voice activation line.
fn meter(ui: &mut Ui, audio: &Audio, s: &Settings) {
    let fraction = |db: f32| ((db - FLOOR) / (CEILING - FLOOR)).clamp(0.0, 1.0);
    let level = audio.level();
    let width = METER_WIDTH;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 10.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
    let open = if s.push_to_talk {
        audio.talking()
    } else {
        level > s.threshold
    };
    let mut filled = rect;
    filled.set_width(rect.width() * fraction(level));
    painter.rect_filled(filled, 4.0, if open { GREEN } else { Color32::GRAY });
    if !s.push_to_talk {
        let x = rect.left() + rect.width() * fraction(s.threshold);
        painter.vline(
            x,
            rect.y_range(),
            Stroke::new(2.0, ui.visuals().strong_text_color()),
        );
    }
    response.widget_info(|| egui::WidgetInfo::labeled(Role::Meter, true, "Mic level"));
}

fn key_picker(ui: &mut Ui, voice: &mut Voice, s: &mut Settings) {
    ui.horizontal(|ui| {
        if voice.picking_key {
            ui.label("Press a key…");
            let pressed = ui.input(|input| {
                input.events.iter().find_map(|event| match event {
                    Event::Key {
                        key, pressed: true, ..
                    } => Some(*key),
                    _ => None,
                })
            });
            match pressed {
                Some(Key::Escape) => voice.picking_key = false,
                Some(key) => {
                    s.key = key.name().to_owned();
                    voice.picking_key = false;
                }
                None => {}
            }
        } else {
            ui.label(format!("Talk key: {}", s.key));
            if ui.button("Change").clicked() {
                voice.picking_key = true;
            }
        }
    });
    ui.weak("Hold it to talk while RustTalk is the active window and you're not typing.");
}

fn calibrate(ui: &mut Ui, voice: &mut Voice, s: &mut Settings) {
    let now = Instant::now();
    let audio = &voice.audio;
    voice.calibration = match voice.calibration.take() {
        Some(Calibration::Quiet(until)) if now >= until => {
            let quiet = audio.record(true).unwrap_or_default();
            Some(Calibration::Speak(now + Duration::from_secs(4), quiet))
        }
        Some(Calibration::Speak(until, quiet)) if now >= until => {
            let speech = audio.record(false).unwrap_or_default();
            Some(Calibration::Done(match audio::calibrate(&quiet, &speech) {
                Some(threshold) => {
                    s.threshold = threshold.clamp(FLOOR, CEILING);
                    "Done. Your mic now opens when you talk.".into()
                }
                None => "Couldn't hear you clearly. Check your mic and try again.".into(),
            }))
        }
        other => other,
    };
    match &voice.calibration {
        Some(Calibration::Quiet(_)) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Stay quiet for a moment…");
            });
        }
        Some(Calibration::Speak(..)) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Say something, like counting to five.").strong());
            });
        }
        done => {
            if let Some(Calibration::Done(text)) = done {
                ui.label(text.as_str());
            }
            if ui.button("Calibrate").clicked() {
                audio.record(true);
                voice.calibration = Some(Calibration::Quiet(now + Duration::from_secs(2)));
            }
        }
    }
}
