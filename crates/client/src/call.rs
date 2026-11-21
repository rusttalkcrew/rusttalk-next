//! Voice in the window: who is in each room, the call bar, the room view,
//! the connection panel and Settings → Voice.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, ColorImage, ComboBox, Event, Frame, Grid, Image, Key, Layout, Margin, RichText,
    Role, Sense, Slider, Stroke, TextureHandle, TextureOptions, Ui, Vec2,
};

use crate::{
    audio::{self, Audio, Settings, StreamId},
    camera::{Camera, Change},
    files,
    home::{Home, Tab, face, pane_title, toast_line},
    net::Command,
    screen::{self, Kind, Options, Target, Video},
    server::{Call, CallState, Id, Screen, Server, Stats},
    share::{self, App},
    ui::{self, Look, section},
};

/// Meter range in dBFS.
const FLOOR: f32 = -70.0;
const CEILING: f32 = -10.0;
const METER_WIDTH: f32 = 360.0;
/// Device pickers in Settings.
pub const PICKER_WIDTH: f32 = 260.0;
/// Width of a person's tile while it shows their camera.
const VIDEO_WIDTH: f32 = 240.0;
/// The live transfer limit.
pub const MAX_LIVE_FILE: u64 = rusttalk_core::messages::MAX_LIVE_FILE_SIZE;
const DIRECT: &str = "Sent directly through the call. Nothing is stored.";
pub const DIRECT_CHAT: &str = "Sent directly to people online now. Nothing is stored.";
/// How long a live offer can be accepted.
const OFFER_SECONDS: i64 = 10 * 60;

mod panel;
pub use panel::{Link, call_bar};

pub struct Voice {
    pub audio: Audio,
    pub call: Option<Call>,
    /// Ping history and packet loss for the connection popup.
    pub link: Link,
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
    /// Files sent through the call, newest last.
    pub live_files: Vec<LiveFile>,
}

/// A file sent live, shown in the call bar or, when `chat`, in a chat.
pub struct LiveFile {
    pub id: Id,
    /// Offered in a text channel or DM rather than in the call.
    pub chat: bool,
    /// Who offers it; `None` for ours.
    pub from: Option<Id>,
    pub name: String,
    pub size: u64,
    pub state: LiveState,
    /// Who is receiving our file.
    pub receivers: Vec<Id>,
}

#[derive(PartialEq)]
pub enum LiveState {
    Offered,
    /// Bytes received so far.
    Saving(u64),
    Saved,
    Failed,
    Sending,
    /// Stopped, cancelled or declined in a chat.
    Ended,
}

pub enum LiveAction {
    Send,
    Save(Id, String),
    /// Hide it here; for ours or a download, also stop it.
    Remove(Id, bool),
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
            link: Link::default(),
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
            live_files: Vec::new(),
        }
    }

    /// The call ended or was left: stop sending audio and forget its shares and video.
    pub fn end_call(&mut self) {
        self.call = None;
        self.audio.set_voice(false);
        self.link = Link::default();
        self.shares.clear();
        self.clear_screens();
    }

    /// Forget this call's video and files: our screen and camera, and what we watch.
    pub fn clear_screens(&mut self) {
        self.live_files.retain(|file| file.chat);
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
        can_send_files: true,
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
            let face = face(ui, &server.dir, &peer.person, &name, 22.0, true);
            if home.voice.speaking(server, &peer.person) {
                ring(ui, face.rect);
            }
            ui.label(RichText::new(&name).size(ui::TEXT_SMALL));
            if peer.connecting {
                ui.label(ui::caption("Connecting…"));
            }
            if !peer.screens.is_empty() {
                live_badge(ui);
            }
        });
        let row = row.response.interact(Sense::click());
        let listening =
            peer.person != server.dir.me && !peer.unknown && home.voice.in_call(server, channel);
        if listening {
            ui::context_menu(&row, |m| {
                stream_menu(m, &home.voice.audio, &(peer.person, None))
            });
        }
        for (sid, app) in &peer.apps {
            let stream = (peer.person, Some(sid.clone()));
            let row = ui.horizontal(|ui| {
                ui.add_space(52.0);
                ui.label(RichText::new(format!("🎵 {app}")).size(ui::TEXT_SMALL));
                if listening && home.voice.audio.volume(&stream).1 {
                    ui.label(ui::caption("Muted"));
                }
            });
            if listening {
                let row = row.response.interact(Sense::click());
                ui::context_menu(&row, |m| stream_menu(m, &home.voice.audio, &stream));
            }
        }
    }
}

fn ring(ui: &Ui, rect: egui::Rect) {
    ui.painter().circle_stroke(
        rect.center(),
        rect.width() / 2.0 + 2.0,
        Stroke::new(2.0, ui::colors(ui).success),
    );
}

/// Volume and mute for a person's voice or one app they share, in its menu.
pub fn stream_menu(m: &mut ui::Menu, audio: &Audio, stream: &StreamId) {
    let (mut volume, mut muted) = audio.volume(stream);
    let before = (volume, muted);
    m.caption("Volume");
    m.ui().horizontal(|ui| {
        ui.add_space(ui::SPACE_S);
        ui.spacing_mut().slider_width = ui.available_width() - 64.0;
        volume_slider(ui, &mut volume);
    });
    if m.add(ui::Item::new("🔇", "Mute").checked(muted)).clicked() {
        muted = !muted;
    }
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

/// Each file offered in the call with what can be done with it.
fn live_files(ui: &mut Ui, list: &[LiveFile], server: Option<&Server>) -> Option<LiveAction> {
    let mut action = None;
    let name = |person: &Id| server.map_or_else(String::new, |s| s.member_name(person));
    for file in list.iter().filter(|file| !file.chat) {
        let size = files::size(file.size);
        Frame::new()
            .fill(ui::colors(ui).side)
            .corner_radius(ui::RADIUS_ROW)
            .inner_margin(Margin::same(ui::SPACE_S as i8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                match (file.from, &file.state) {
                    (None, _) => {
                        ui.label(format!("Sending {} ({size})", file.name))
                            .on_hover_text(DIRECT);
                        action = sending(ui, file, name).or(action.take());
                    }
                    (Some(from), LiveState::Offered) => {
                        ui.label(format!(
                            "{} wants to send {} ({size})",
                            name(&from),
                            file.name
                        ))
                        .on_hover_text(DIRECT);
                        action =
                            receiving(ui, file.id, &file.name, file.size, None).or(action.take());
                    }
                    (Some(_), LiveState::Saving(bytes)) => {
                        ui.label(&file.name);
                        action = receiving(ui, file.id, &file.name, file.size, Some(*bytes))
                            .or(action.take());
                    }
                    (Some(_), state) => {
                        ui.horizontal(|ui| {
                            ui.label(if *state == LiveState::Saved {
                                format!("Saved {}", file.name)
                            } else {
                                format!("{} didn't arrive.", file.name)
                            });
                            if ui::small_close(ui, "Dismiss").clicked() {
                                action = Some(LiveAction::Remove(file.id, false));
                            }
                        });
                    }
                }
            });
    }
    action
}

/// Who receives our live file, and "Stop sending".
fn sending(ui: &mut Ui, file: &LiveFile, name: impl Fn(&Id) -> String) -> Option<LiveAction> {
    let who: Vec<_> = file.receivers.iter().map(name).collect();
    ui.label(ui::caption(match who.len() {
        0 => "Waiting for someone to save it".to_owned(),
        _ => format!("{} receiving", who.join(", ")),
    }));
    let stop = ui::secondary(ui, "Stop sending").clicked();
    stop.then_some(LiveAction::Remove(file.id, true))
}

/// "Save" and "Decline" for a live file offered to us, or, with the bytes `saved`
/// so far, its progress and "Cancel". Used in the call and in chats.
fn receiving(ui: &mut Ui, id: Id, name: &str, size: u64, saved: Option<u64>) -> Option<LiveAction> {
    let mut action = None;
    match saved {
        None => {
            ui.horizontal(|ui| {
                if ui::primary(ui, "Save").clicked() {
                    action = Some(LiveAction::Save(id, name.to_owned()));
                }
                if ui::secondary(ui, "Decline").clicked() {
                    action = Some(LiveAction::Remove(id, false));
                }
            });
        }
        Some(bytes) => {
            let done = bytes as f32 / size.max(1) as f32;
            ui.add(egui::ProgressBar::new(done).show_percentage());
            if ui::secondary(ui, "Cancel").clicked() {
                action = Some(LiveAction::Remove(id, true));
            }
        }
    }
    action
}

/// A live file offered in a text channel or DM, inside its message.
pub fn chat_live(
    ui: &mut Ui,
    list: &[LiveFile],
    dir: &crate::server::Directory,
    msg: &crate::server::Msg,
) -> Option<LiveAction> {
    let (id, name, size) = msg.live.as_ref()?;
    let entry = list.iter().find(|file| file.id == *id);
    let mine = msg.person == dir.me;
    let fresh = chrono::Utc::now().timestamp() - msg.at < OFFER_SECONDS;
    let mut action = None;
    files::attachment_frame(ui).show(ui, |ui| {
        ui.set_max_width(360.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("📡").size(ui::TEXT_ICON_LARGE))
                .on_hover_text(DIRECT_CHAT);
            ui.vertical(|ui| {
                ui.label(ui::bold(name));
                ui.label(ui::caption(format!("{} · Live", files::size(*size))))
                    .on_hover_text(DIRECT_CHAT);
                let state = entry.map(|file| &file.state);
                match (mine, state) {
                    (true, Some(LiveState::Sending)) => {
                        let file = entry.expect("matched a state");
                        action = sending(ui, file, |p| crate::server::member_name(dir, p));
                    }
                    (_, Some(LiveState::Saving(bytes))) => {
                        action = receiving(ui, *id, name, *size, Some(*bytes));
                    }
                    (_, Some(LiveState::Saved)) => {
                        ui.weak("Saved");
                    }
                    (_, Some(LiveState::Failed)) => {
                        ui::error_text(ui, Some("Didn't arrive."));
                    }
                    (false, None | Some(LiveState::Offered)) if fresh => {
                        action = receiving(ui, *id, name, *size, None);
                    }
                    (_, Some(LiveState::Ended)) => {
                        ui.weak("Stopped");
                    }
                    _ => {
                        ui.weak("No longer available");
                    }
                }
            });
        });
    });
    action
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
    let ((), dismissed) = ui::dialog(ctx, "share screen", "Share your screen", |ui| {
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
        ui::checkbox(ui, &mut options.cursor, "Show my cursor");
        ui::footer(ui, |ui| {
            let label = if portal.is_some() {
                "Choose what to share"
            } else {
                "Share"
            };
            let target = match &*targets {
                Listing::Found(found) => portal.or(*picked).and_then(|at| found.get(at)),
                _ => None,
            };
            let button = ui.add_enabled_ui(target.is_some(), |ui| ui::primary(ui, label));
            if button.inner.clicked() {
                share = target.cloned();
            }
            close |= ui::secondary(ui, "Cancel").clicked();
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
    if close || dismissed {
        home.voice.share_screen = None;
    }
}

/// A red "LIVE" tag for someone sharing their screen.
fn live_badge(ui: &mut Ui) {
    ui::badge(ui, "LIVE", ui::colors(ui).danger);
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
        ui.label(ui::bold(&watching.title));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            stop = ui::secondary(ui, "Stop watching").clicked();
            let label = if watching.full {
                "Exit full window"
            } else {
                "Full window"
            };
            if ui::secondary(ui, label).clicked() {
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

/// The main pane for a voice channel: everyone in it, with speaking rings.
pub fn room(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    let server = &home.servers[index];
    let title = server
        .chat_title(&channel)
        .map(|(name, _)| name)
        .unwrap_or_default();
    let joined = home.voice.in_call(server, &channel);
    pane_title(ui, "room-title", &format!("🔊 {title}"));
    // Problems from the call bar and the room show here; the chat has its own line.
    toast_line(ui, &mut home.toast);
    ui.add_space(ui::SPACE_S);
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
                .fill(ui::colors(ui).side)
                .corner_radius(ui::RADIUS)
                .inner_margin(Margin::same(ui::SPACE_M as i8))
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
                            None => face(ui, &server.dir, &peer.person, &name, 64.0, false),
                        };
                        if home.voice.speaking(server, &peer.person) {
                            if video {
                                ui.painter().rect_stroke(
                                    face.rect,
                                    4.0,
                                    Stroke::new(2.0, ui::colors(ui).success),
                                    egui::StrokeKind::Outside,
                                );
                            } else {
                                ring(ui, face.rect);
                            }
                        }
                        ui.label(ui::bold(&name));
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
                ui::context_menu(&tile, |m| {
                    stream_menu(m, &home.voice.audio, &(peer.person, None));
                    if peer.camera.is_some() {
                        let show = !home.voice.hidden.contains(&peer.person);
                        let item = ui::Item::new("📷", "Show video").checked(show);
                        if m.add(item).clicked() {
                            hide = Some((peer.person, show));
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
                        if ui::secondary(ui, "Stop sharing").clicked() {
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
        if ui::primary_large(ui, "Join voice").clicked() {
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
    let look = if muted { Look::Struck } else { Look::Plain };
    let mute = ui::icon_button(
        ui,
        icon,
        &format!("{label} {name}"),
        look,
        Vec2::splat(30.0),
    );
    if slider.changed() || mute.clicked() {
        audio.set_volume(stream, volume, muted ^ mute.clicked());
    }
}

/// Settings → Voice & audio.
pub fn settings_tab(ui: &mut Ui, voice: &mut Voice) {
    let audio = voice.audio.clone();
    let mut s = audio.settings();
    let (inputs, outputs) = audio.devices();
    ui::page_section(ui, "Devices");
    device_picker(ui, "Microphone", &mut s.input, &inputs);
    device_picker(ui, "Speakers", &mut s.output, &outputs);
    if let Some(problem) = audio.problem() {
        ui::error_text(ui, Some(&problem));
    }
    ui::page_section(ui, "Input mode");
    ui.horizontal_wrapped(|ui| {
        let cards = [
            (false, "🔊", "Voice activity", "Talk when you speak"),
            (true, "⌨", "Push to talk", "Hold a key to talk"),
        ];
        for (push_to_talk, icon, title, help) in cards {
            let selected = s.push_to_talk == push_to_talk;
            if ui::choice_card(ui, selected, icon, title, help).clicked() {
                s.push_to_talk = push_to_talk;
            }
        }
    });
    ui.add_space(4.0);
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
    ui::page_section(ui, "Voice processing");
    ui::toggle_row(
        ui,
        &mut s.echo_cancellation,
        "Echo cancellation",
        "Stops others from hearing themselves through your speakers.",
    );
    ui::toggle_row(
        ui,
        &mut s.noise_suppression,
        "Noise suppression",
        "Filters out background noise like fans and typing.",
    );
    ui::toggle_row(
        ui,
        &mut s.auto_gain,
        "Automatic gain",
        "Keeps your voice at an even volume.",
    );
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
    ui::setting_row(ui, label, "", |ui| {
        let combo = ComboBox::from_id_salt(label)
            .width(PICKER_WIDTH)
            .selected_text(current)
            .show_ui(ui, |ui| {
                ui.selectable_value(chosen, None, "System default");
                for device in devices {
                    ui.selectable_value(chosen, Some(device.id.clone()), &device.name);
                }
            });
        combo
            .response
            .widget_info(|| egui::WidgetInfo::labeled(Role::ComboBox, true, label));
    });
}

/// The live input level, with the voice activation line.
fn meter(ui: &mut Ui, audio: &Audio, s: &Settings) {
    let fraction = |db: f32| ((db - FLOOR) / (CEILING - FLOOR)).clamp(0.0, 1.0);
    let level = audio.level();
    let width = METER_WIDTH;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 10.0), Sense::hover());
    let painter = ui.painter();
    let c = ui::colors(ui);
    painter.rect_filled(rect, ui::RADIUS_S, c.field);
    let open = if s.push_to_talk {
        audio.talking()
    } else {
        level > s.threshold
    };
    let mut filled = rect;
    filled.set_width(rect.width() * fraction(level));
    painter.rect_filled(
        filled,
        ui::RADIUS_S,
        if open { c.success } else { c.offline },
    );
    if !s.push_to_talk {
        let x = rect.left() + rect.width() * fraction(s.threshold);
        painter.vline(x, rect.y_range(), Stroke::new(2.0, c.strong));
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
            if ui::secondary(ui, "Change").clicked() {
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
                ui.label(ui::bold("Say something, like counting to five."));
            });
        }
        done => {
            if let Some(Calibration::Done(text)) = done {
                ui.label(text.as_str());
            }
            if ui::secondary(ui, "Calibrate").clicked() {
                audio.record(true);
                voice.calibration = Some(Calibration::Quiet(now + Duration::from_secs(2)));
            }
        }
    }
}
