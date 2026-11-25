//! Voice in the window: who is in each room, the call bar, the room view,
//! the connection panel and Settings → Voice.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use eframe::egui::{
    self, Align, ColorImage, ComboBox, Event, Frame, Key, Label, Layout, Margin, Rect, Response,
    RichText, Role, Sense, Slider, Stroke, TextureHandle, TextureOptions, Ui, UiBuilder, Vec2,
    WidgetInfo,
};
use rusttalk_core::client::voice::VoiceStatus;

use crate::{
    audio::{self, Audio, Settings},
    camera::{Camera, Change},
    denoise, files,
    home::{
        Home, Tab,
        person::{self, Person, Pick},
    },
    mic_check::Verdict,
    net::{Admin, Command, Update},
    screen::{self, Kind, Options, Target, Video},
    server::{Call, CallState, Id, Screen, Server, Stats, Status},
    share::{self, App},
    sounds,
    ui::{self, section},
};

/// Meter range in dBFS.
const FLOOR: f32 = -70.0;
const CEILING: f32 = -10.0;
const METER_WIDTH: f32 = 360.0;
/// Device pickers in Settings.
pub const PICKER_WIDTH: f32 = 260.0;
/// The live transfer limit.
pub const MAX_LIVE_FILE: u64 = rusttalk_core::messages::MAX_LIVE_FILE_SIZE;
/// How long a live offer can be accepted.
const OFFER_SECONDS: i64 = 10 * 60;

pub mod mixer;
mod overlay;
mod panel;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub mod phone;
pub mod remote;
mod stage;
pub mod terminal;
pub use panel::{Link, call_bar, phone_controls};
pub use stage::{Stage, full_view, stage};

pub struct Voice {
    pub external_phones: Vec<(
        rusttalk_core::client::voice::phone::Call,
        String,
        crate::phone::ConsentStatus,
    )>,
    pub audio: Audio,
    pub call: Option<Call>,
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub(crate) pending_invitation: Option<Id>,
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pending_join: Option<(url::Url, Id, bool)>,
    pub sounds: sounds::Detector,
    /// Ping history and packet loss for the connection popup.
    pub link: Link,
    /// Apps this device shares in the call.
    pub shares: Vec<App>,
    /// The sound the screen share carries (or will): apps, all system audio, or none.
    pub stream_audio: Vec<App>,
    /// Apps playing sound when the picker opened, filled by a listing thread.
    apps: Arc<Mutex<Listing<App>>>,
    supported: bool,
    calibration: Option<Calibration>,
    picking_key: bool,
    pub video: Video,
    pub sharing: Sharing,
    /// The dialog's choices: options and the picked entry of `targets`.
    share_screen: Option<(Options, Option<usize>)>,
    /// The dialog switches the screen being shared, keeping its quality, rather than starting one.
    changing_screen: bool,
    /// Screens and windows to pick from (Windows), filled by a listing thread.
    targets: Arc<Mutex<Listing<Target>>>,
    pub watching: Option<Watching>,
    /// The newest picture of our own share, while its preview is on.
    preview: Option<TextureHandle>,
    pub camera: Camera,
    /// People whose camera this device chose not to receive.
    pub(crate) hidden: HashSet<Id>,
    /// Camera tracks this device receives, with their newest picture.
    cameras: HashMap<String, Option<TextureHandle>>,
    /// Camera tracks drawn this frame; only these stay subscribed.
    shown: HashSet<String>,
    /// Files sent through the call, newest last.
    pub live_files: Vec<LiveFile>,
    /// Focus, minimized people and presentation mode, for this call only.
    pub stage: Stage,
    /// Remote control of our screen, or of one we watch.
    pub control: remote::Control,
    /// Our shared terminal, and the one we watch.
    pub terminals: terminal::Terminals,
    /// A hint about the microphone, once per call, until answered.
    pub mic_hint: Option<Verdict>,
    /// A phone call into the room, and the dialer.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub phone: phone::Phone,
    highlight: Option<Highlight>,
}

/// A Voice & audio row a microphone hint points to.
#[derive(Clone, Copy, PartialEq)]
pub enum Row {
    AiNoise,
    Gain,
}

/// Shows the row briefly after Settings opens from a hint.
pub struct Highlight {
    row: Row,
    /// `ui.input` time of the first frame the row was drawn.
    since: Option<f64>,
}

/// How long the row stays highlighted, fading out.
const HIGHLIGHT_SECONDS: f64 = 3.0;

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
    /// Stopped or cancelled in a chat.
    Ended,
}

pub enum LiveAction {
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
    /// Picking another screen; the current one keeps sharing meanwhile.
    Changing,
    On,
}

/// The screen this device receives; one at a time.
pub struct Watching {
    pub sid: String,
    missing_since: Option<Instant>,
    /// Brief window after `Reconnecting -> Connected` for a remote sharer to republish.
    pub(crate) grace_until: Option<Instant>,
    texture: Option<TextureHandle>,
}

enum Calibration {
    /// Until when.
    Quiet(Instant),
    Speak(Instant, Vec<f32>),
    Done(String),
}

impl Watching {
    /// Keep watch intent while reconnecting (bounded to 180s) or during the brief
    /// post-recovery grace window, never during a normal stop-share in a healthy call.
    fn expired(&mut self, shared: bool, reconnecting: bool, now: Instant) -> bool {
        if shared {
            self.missing_since = None;
            self.grace_until = None;
            return false;
        }
        if reconnecting {
            return now.saturating_duration_since(*self.missing_since.get_or_insert(now))
                >= Duration::from_secs(180);
        }
        if let Some(until) = self.grace_until {
            return now >= until;
        }
        true
    }
}

impl Voice {
    pub(crate) fn retaining_watch(&self, sid: &str) -> bool {
        let reconnecting = self
            .call
            .as_ref()
            .is_some_and(|call| call.state == CallState::Reconnecting);
        self.watching
            .as_ref()
            .is_some_and(|w| w.sid == sid && (reconnecting || w.grace_until.is_some()))
    }

    pub fn replace_video(&mut self, old: &str, new: &str) {
        if let Some(watching) = &mut self.watching
            && watching.sid == old
        {
            watching.sid = new.to_owned();
            watching.texture = None;
            watching.missing_since = None;
        }
        if self.cameras.remove(old).is_some() {
            self.cameras.entry(new.to_owned()).or_insert(None);
        }
        if self.shown.remove(old) {
            self.shown.insert(new.to_owned());
        }
        self.stage.replace_video(old, new);
    }

    pub fn new(audio: Audio, video: Video, runtime: tokio::runtime::Handle) -> Self {
        Self {
            audio,
            call: None,
            #[cfg(any(target_os = "android", target_os = "ios"))]
            pending_invitation: None,
            #[cfg(any(target_os = "android", target_os = "ios"))]
            pending_join: None,
            sounds: sounds::Detector::default(),
            link: Link::default(),
            shares: Vec::new(),
            stream_audio: Vec::new(),
            apps: Arc::new(Mutex::new(Listing::Loading)),
            supported: share::supported(),
            calibration: None,
            picking_key: false,
            video,
            sharing: Sharing::Off,
            share_screen: None,
            changing_screen: false,
            targets: Arc::new(Mutex::new(Listing::Loading)),
            watching: None,
            preview: None,
            camera: Camera::new(runtime),
            hidden: HashSet::new(),
            cameras: HashMap::new(),
            shown: HashSet::new(),
            live_files: Vec::new(),
            stage: Stage::default(),
            control: remote::Control::default(),
            terminals: terminal::Terminals::default(),
            mic_hint: None,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            phone: phone::Phone::default(),
            external_phones: Vec::new(),
            highlight: None,
        }
    }

    /// Open Voice & audio scrolled to `row`, which flashes.
    pub fn show_row(&mut self, row: Row) {
        self.highlight = Some(Highlight { row, since: None });
    }

    /// The call ended or was left: stop sending audio and forget its shares and video.
    pub fn end_call(&mut self) {
        #[cfg(target_os = "android")]
        crate::android::communication_mode(false);
        self.call = None;
        self.audio.end_voice(self.sounds.end());
        self.external_phones.clear();
        self.link = Link::default();
        self.shares.clear();
        self.clear_screens();
        self.stage = Stage::default();
        self.control = remote::Control::default();
        // Leaving the call ends our shared shell.
        self.terminals = terminal::Terminals::default();
        self.mic_hint = None;
        let _ = self.audio.take_trouble();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.phone.end();
    }

    /// Forget this call's video and files: our screen and camera, and what we watch.
    pub fn clear_screens(&mut self) {
        self.live_files.retain(|file| file.chat);
        self.sharing = Sharing::Off;
        self.share_screen = None;
        self.changing_screen = false;
        self.watching = None;
        self.stop_preview();
        self.camera.reset();
        self.cameras.clear();
    }

    pub(crate) fn default_stream_audio(&self) -> Vec<App> {
        if self.supported {
            vec![share::system()]
        } else {
            Vec::new()
        }
    }

    pub(crate) fn update_preview(&mut self, ctx: &egui::Context) {
        let active = ctx.input(|i| i.focused && i.viewport().minimized != Some(true));
        let on = matches!(self.sharing, Sharing::On | Sharing::Changing) && active;
        if self.video.preview.is_on() != on {
            self.video.preview.set(on);
        }
        if !on {
            self.preview = None;
        }
    }

    /// Clear the preview when the screen share ends.
    pub fn stop_preview(&mut self) {
        self.video.preview.set(false);
        self.preview = None;
    }

    pub(crate) fn in_call(&self, server: &Server, channel: &Id) -> bool {
        self.call
            .as_ref()
            .is_some_and(|call| call.server == server.address && call.channel == *channel)
    }

    /// Whether this device is actively transmitting voice in its current call.
    pub(crate) fn local_speaking(&self, servers: &[Server]) -> bool {
        let Some(call) = &self.call else {
            return false;
        };
        let Some(server) = servers.iter().find(|s| s.address == call.server) else {
            return false;
        };
        self.local_speaking_in(server, &call.channel)
    }

    fn local_speaking_in(&self, server: &Server, channel: &Id) -> bool {
        let Some(call) = &self.call else {
            return false;
        };
        call.server == server.address
            && call.channel == *channel
            && call.state == CallState::Connected
            && call.can_talk
            && !server.voice_muted(&server.dir.me)
            && !self.audio.channel_mic_paused()
            && self.audio.talking()
    }

    fn speaking(&self, server: &Server, channel: &Id, person: &Id) -> bool {
        if *person == server.dir.me {
            self.local_speaking_in(server, channel)
        } else {
            self.in_call(server, channel) && self.audio.speaking(person)
        }
    }
}

/// Join a voice channel, leaving any other room first, and show it. A channel in focus
/// mode shows its notice first.
pub fn join(home: &mut Home, index: usize, channel: Id) {
    // Previewing the server as other roles: joining would change something.
    if !home.voice.in_call(&home.servers[index], &channel)
        && let Some(view) = &home.servers[index].view_as
    {
        view.refuse();
        return;
    }
    if !home.voice.in_call(&home.servers[index], &channel)
        && crate::home::focus::ask_first(home, index, channel)
    {
        return;
    }
    join_now(home, index, channel);
}

/// `join` without the focus mode notice, once it was accepted.
pub fn join_now(home: &mut Home, index: usize, channel: Id) {
    // Previewing the server as other roles: joining would change something.
    if !home.voice.in_call(&home.servers[index], &channel)
        && let Some(view) = &home.servers[index].view_as
    {
        view.refuse();
        return;
    }
    cancel_invitation_join(home);
    home.servers[index].open = Some(channel);
    if home.voice.in_call(&home.servers[index], &channel) {
        #[cfg(any(target_os = "android", target_os = "ios"))]
        {
            home.voice.pending_join = None;
        }
        return;
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let request = home
            .voice
            .pending_join
            .as_ref()
            .is_none_or(|pending| pending.2);
        home.voice.pending_join = Some((home.servers[index].address.clone(), channel, request));
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if phone::leave_or_ask(home, phone::Leave::Join(index, channel)) {
            return;
        }
        leave(home);
        connect(home, index, channel);
    }
}

pub(crate) fn join_ready(home: &mut Home, index: usize, channel: Id, _now: f64) -> bool {
    leave(home);
    #[cfg(target_os = "ios")]
    if let Err(error) = home.voice.audio.prepare_ios_voice() {
        tracing::warn!(%error, "iOS voice session failed");
        home.toast = Some((t!("voice.unavailable").into(), _now + 6.0));
        return false;
    }
    home.servers[index].open = Some(channel);
    connect(home, index, channel);
    true
}

fn connect(home: &mut Home, index: usize, channel: Id) {
    #[cfg(target_os = "android")]
    crate::android::communication_mode(true);
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
    home.voice.audio.start_voice();
}

// A late destination check must never override a newer call decision.
fn cancel_invitation_join(home: &mut Home) {
    home.requests
        .incoming
        .retain(|request| request.check == crate::voice_requests::JoinCheck::Idle);
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        home.voice.pending_invitation = None;
    }
}

pub fn leave(home: &mut Home) {
    cancel_invitation_join(home);
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        home.voice.pending_join = None;
    }
    let Some(call) = home.voice.call.take() else {
        return;
    };
    home.voice.end_call();
    if let Some(server) = home.servers.iter().find(|s| s.address == call.server) {
        server.send(Command::LeaveVoice);
    }
}

/// When the call needs its next frame without input or network news. The audio thread
/// wakes the UI when someone starts speaking; this turns the ring off on time, keeps the
/// settings meter live while it can be seen, and catches slow timeouts once a second.
fn tick(
    in_call: bool,
    testing: bool,
    focused: bool,
    speaking_ends: Option<Duration>,
) -> Option<Duration> {
    let meter = (testing && focused).then_some(Duration::from_millis(100));
    let fallback = (in_call || testing).then_some(Duration::from_secs(1));
    [meter, speaking_ends, fallback].into_iter().flatten().min()
}

/// Once per frame: push-to-talk, the settings meter, and repaints for live indicators.
pub fn frame(ctx: &egui::Context, home: &mut Home) {
    #[cfg(target_os = "ios")]
    if home.voice.call.is_some() && crate::ios::voice::needs_rejoin() {
        leave(home);
        home.toast = Some((t!("voice.audio_reset").into(), ctx.input(|i| i.time) + 6.0));
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    if let Some((address, channel, request)) = home.voice.pending_join.take() {
        #[cfg(target_os = "android")]
        use crate::android::voice_permission;
        #[cfg(target_os = "ios")]
        use crate::ios::voice::voice_permission;
        if let Some(id) = home.voice.pending_invitation
            && !home.requests.incoming.iter().any(|r| {
                r.id == id
                    && r.server == address
                    && r.channel == channel
                    && r.until > crate::notify::unix_now()
            })
        {
            home.voice.pending_invitation = None;
            return;
        }
        match voice_permission(request) {
            0 => {
                home.voice.pending_join = Some((address, channel, false));
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            1 => {
                if let Some(index) = home.servers.iter().position(|s| {
                    s.address == address
                        && s.status == crate::server::Status::Online
                        && s.is_voice(&channel)
                }) {
                    if let Some(id) = home.voice.pending_invitation.take() {
                        home.check_voice_destination(index, id);
                    } else {
                        join_ready(home, index, channel, ctx.input(|i| i.time));
                    }
                }
            }
            result => {
                let message = if result == -1 {
                    t!("voice.need_mic")
                } else {
                    t!("voice.unavailable")
                };
                home.toast = Some((message.into(), ctx.input(|i| i.time) + 6.0));
            }
        }
    }
    home.voice.update_preview(ctx);
    let recording = home.hotkeys.recording();
    let voice = &mut home.voice;
    let mut settings = voice.audio.settings();
    let key = Key::from_name(&settings.key);
    // While another app is in front, the global shortcut (see `hotkeys`) talks instead.
    let down = settings.push_to_talk
        && !voice.picking_key
        && !recording
        && !ctx.egui_wants_keyboard_input()
        && key.is_some_and(|key| ctx.input(|input| input.key_down(key)));
    voice.audio.set_key_down(down);
    let testing = home.settings == Some(Tab::Voice);
    voice.audio.set_testing(testing);
    if voice.call.is_some()
        && let Some(verdict) = voice.audio.take_trouble()
    {
        voice.mic_hint = Some(verdict);
    }
    if !testing && voice.highlight.as_ref().is_some_and(|h| h.since.is_some()) {
        voice.highlight = None;
    }
    let focused = ctx.input(|input| input.focused);
    let speaking_ends = voice.audio.speaking_ends();
    if let Some(after) = tick(voice.call.is_some(), testing, focused, speaking_ends) {
        ctx.request_repaint_after(after);
    }
    remote::frame(home, ctx.input(|input| input.time));
    terminal::frame(home, ctx.input(|input| input.time));
    // Keep watch intent through a bounded outage while the owner republishes.
    if let Some(watching) = &home.voice.watching {
        let reconnecting = home
            .voice
            .call
            .as_ref()
            .is_some_and(|call| call.state == CallState::Reconnecting);
        let shared = call_server(home)
            .zip(home.voice.call.as_ref())
            .and_then(|(server, call)| server.voice.get(&call.channel))
            .is_some_and(|peers| {
                peers
                    .iter()
                    .any(|peer| peer.screens.iter().any(|s| s.sid == watching.sid))
            });
        let watching = home.voice.watching.as_mut().expect("checked above");
        if watching.expired(shared, reconnecting, Instant::now()) {
            stop_watching(home);
        }
    }
    let can_video = home
        .voice
        .call
        .as_ref()
        .is_some_and(|call| call.can_share_video);
    let preview = home.settings == Some(Tab::Video);
    let change = home
        .voice
        .camera
        .update(ctx, &mut settings, can_video, preview);
    #[cfg(any(target_os = "android", target_os = "ios"))]
    home.voice.audio.set_settings(settings.clone());
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

pub(crate) fn call_server(home: &Home) -> Option<&Server> {
    let call = home.voice.call.as_ref()?;
    home.servers.iter().find(|s| s.address == call.server)
}

fn watch(home: &mut Home, screen: &Screen) {
    stop_watching(home);
    if let Some(server) = call_server(home) {
        server.send(Command::Watch(screen.sid.clone(), true));
        home.voice.watching = Some(Watching {
            sid: screen.sid.clone(),
            missing_since: None,
            grace_until: None,
            texture: None,
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

/// Room the Join button takes at a voice row's right edge, so the name is cut short first.
pub const JOIN_SLOT: f32 = 30.0;
/// Room for "3/5" before the Join button, on voice rows with a member limit.
pub const COUNT_SLOT: f32 = 34.0;

/// How full a voice channel with a member limit is, like "3/5", just before the Join slot.
pub fn member_count(ui: &mut Ui, server: &Server, channel: &Id, limit: u32, row: &Response) {
    let people = server.voice.get(channel).map_or(0, Vec::len);
    let right = row.rect.right() - JOIN_SLOT;
    let slot = Rect::from_x_y_ranges(right - COUNT_SLOT..=right, row.rect.y_range());
    let mut at = ui.new_child(
        UiBuilder::new()
            .max_rect(slot)
            .layout(Layout::right_to_left(Align::Center)),
    );
    let text = t!("call.member_count", people = people, limit = limit);
    at.add(Label::new(ui::caption(text)).selectable(false))
        .on_hover_text(t!("call.member_count_tip", people = people, limit = limit));
}

/// The Join button on a voice channel's row: shown while the row is hovered or open,
/// hidden when this is the call you are in (the call bar has Leave). True when clicked.
pub fn join_button(
    ui: &mut Ui,
    home: &Home,
    server: &Server,
    channel: &Id,
    row: &Response,
    selected: bool,
) -> bool {
    let shown = selected || ui.rect_contains_pointer(row.rect);
    if !shown
        || server.status != Status::Online
        || server.view_as.is_some()
        || home.voice.in_call(server, channel)
    {
        return false;
    }
    let size = Vec2::splat(ui::SPACE_XL);
    let center = egui::pos2(row.rect.right() - JOIN_SLOT / 2.0, row.rect.center().y);
    // A child Ui, so the button doesn't move the layout cursor away from the row's end.
    let mut at = ui.new_child(UiBuilder::new().max_rect(Rect::from_center_size(center, size)));
    ui::icon_button(&mut at, "📞", t!("call.join_voice"), ui::Look::Ghost, size).clicked()
}

/// The people in a voice channel, under its name in the channel list. A click on
/// someone shows their profile card, a right click their menu; returns what was picked.
pub fn room_list(ui: &mut Ui, home: &Home, server: &Server, channel: &Id) -> Option<(Id, Pick)> {
    let peers = server.voice.get(channel)?;
    let mut pick = None;
    // The speaking ring reaches 3 points past the picture: keep rows clear of each other.
    ui.spacing_mut().item_spacing.y = ui::SPACE_S;
    for peer in peers {
        let name = peer.name(server);
        let row = ui.horizontal(|ui| {
            ui.add_space(22.0);
            let speaking = home.voice.speaking(server, channel, &peer.person);
            crate::home::face_speaking(ui, &server.dir, &peer.person, &name, 22.0, true, speaking);
            let status = if peer.person == server.dir.me && home.voice.in_call(server, channel) {
                VoiceStatus {
                    muted: home.voice.audio.display_muted(),
                    deafened: home.voice.audio.deafened(),
                }
            } else {
                peer.status
            };
            let after_name = |ui: &mut Ui| {
                if peer.connecting {
                    ui.label(ui::caption(t!("common.connecting")));
                }
                if !peer.screens.is_empty() || peer.terminal.is_some() {
                    live_badge(ui);
                }
            };
            // Mute and deafen sit at the row's right edge; the name, and LIVE right after
            // it, fill what is left and the name shortens to keep clear of them.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(ui::SPACE_S);
                // A moderator's mute replaces the person's own mute mark.
                let server_muted = server.voice_muted(&peer.person);
                let own = VoiceStatus {
                    muted: status.muted && !server_muted,
                    ..status
                };
                let marks: Vec<_> = sound_badges(own).collect();
                for (glyph, tip) in marks.into_iter().rev() {
                    sound_mark(ui, glyph, tip);
                }
                if server_muted {
                    server_mute_mark(ui);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let mut measure = ui.new_child(UiBuilder::new().sizing_pass().invisible());
                    after_name(&mut measure);
                    let width = measure.min_rect().width();
                    let reserved = if width > 0.0 {
                        width + ui.spacing().item_spacing.x
                    } else {
                        0.0
                    };
                    ui.scope(|ui| {
                        ui.set_max_width((ui.available_width() - reserved).max(0.0));
                        // Not selectable, so a right-click on the name opens the row's menu.
                        let label = Label::new(RichText::new(&name).size(ui::TEXT_SMALL));
                        ui.add(label.truncate().selectable(false));
                    });
                    after_name(ui);
                });
            });
        });
        let row =
            (row.response.interact(Sense::click())).on_hover_cursor(egui::CursorIcon::PointingHand);
        row.widget_info(|| WidgetInfo::labeled(Role::Button, true, &name));
        if !peer.unknown
            && let Some(picked) =
                person::interact(ui, &row, &Person::of(home, server, peer.person), false)
        {
            pick = Some((peer.person, picked));
        }
        let listening = peer.person != server.dir.me && home.voice.in_call(server, channel);
        for shared in &peer.apps {
            let app = &shared.1;
            let stream = (peer.person, Some(app.clone()));
            let row = ui.horizontal(|ui| {
                ui.add_space(52.0);
                ui.label(RichText::new(format!("🎵 {app}")).size(ui::TEXT_SMALL));
                if listening && home.voice.audio.stream_muted(&stream) {
                    ui.label(ui::caption(t!("call.muted")));
                }
            });
            if listening {
                let row = row.response.interact(Sense::click());
                let apps = std::slice::from_ref(shared);
                ui::context_menu(&row, |m| {
                    mixer::show(m, &home.voice.audio, peer.person, false, apps, |sid, on| {
                        server.send(Command::Listen(sid, on))
                    });
                });
            }
        }
    }
    pick
}

/// Mute for everyone, move and remove, as far as this moderator may. The server also
/// checks that they rank above the person.
pub fn moderation_menu(
    m: &mut ui::Menu,
    server: &Server,
    channel: &Id,
    person: Id,
) -> Option<Admin> {
    let perms = server.dir.perms;
    let mut request = None;
    if perms.mute_members {
        let muted = server.voice_muted(&person);
        let (icon, label) = if muted {
            ("🎤", t!("call.unmute_everyone"))
        } else {
            ("🔇", t!("call.mute_everyone"))
        };
        if m.item(icon, label).clicked() {
            request = Some(Admin::VoiceMute(person, !muted));
        }
    }
    if perms.move_members {
        let others: Vec<_> = (server.dir.categories.iter())
            .flat_map(|category| &category.channels)
            .filter(|other| other.voice && other.id != *channel)
            .collect();
        if !others.is_empty() {
            m.submenu("↪", t!("call.move_to"), |m| {
                for other in others {
                    if m.item("🔊", &other.name).clicked() {
                        request = Some(Admin::VoiceMove {
                            person,
                            from: *channel,
                            to: Some(other.id),
                        });
                    }
                }
            });
        }
        if m.danger("🚪", t!("call.remove_from_channel")).clicked() {
            request = Some(Admin::VoiceMove {
                person,
                from: *channel,
                to: None,
            });
        }
    }
    request
}

fn sound_badges(status: VoiceStatus) -> impl Iterator<Item = (&'static str, &'static str)> + Clone {
    [
        status.muted.then_some(("🎤", t!("call.muted"))),
        status.deafened.then_some(("🎧", t!("call.deafened"))),
    ]
    .into_iter()
    .flatten()
}

/// A name with its sound badges. A moderator's mute replaces the person's own mute badge.
fn status_name(ui: &mut Ui, name: &str, mut status: VoiceStatus, server_muted: bool) {
    status.muted &= !server_muted;
    let badges = sound_badges(status);
    let count = badges.clone().count() + usize::from(server_muted);
    let reserved = count as f32 * (ui::TEXT_BODY + ui.spacing().item_spacing.x);
    ui.scope(|ui| {
        ui.set_max_width((ui.available_width() - reserved).max(0.0));
        // Not selectable, so a right-click on the name opens the row's menu.
        let label = Label::new(RichText::new(name).size(ui::TEXT_SMALL));
        ui.add(label.truncate().selectable(false));
    });
    if server_muted {
        server_mute_mark(ui);
    }
    for (glyph, tip) in badges {
        sound_mark(ui, glyph, tip);
    }
}

/// A moderator's mute: a red muted speaker, unlike the struck mic of a person's own mute.
fn server_mute_mark(ui: &mut Ui) {
    let tip = t!("call.server_muted");
    let glyph = RichText::new("🔇").size(ui::TEXT_SMALL);
    ui.label(glyph.color(ui::colors(ui).danger_text))
        .on_hover_text(tip)
        .widget_info(|| WidgetInfo::labeled(Role::Image, true, tip));
}

/// A struck-through microphone or headphones.
fn sound_mark(ui: &mut Ui, glyph: &str, tip: &str) {
    let response = ui.label(ui::caption(glyph)).on_hover_text(tip);
    let half = Vec2::splat(ui::SPACE_XS);
    ui.painter().line_segment(
        [response.rect.center() - half, response.rect.center() + half],
        Stroke::new(2.0, ui::colors(ui).danger_text),
    );
    response.widget_info(|| WidgetInfo::labeled(Role::Image, true, tip));
}

fn ring(ui: &Ui, rect: egui::Rect) {
    ui::speaking_outline(ui, rect);
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
                        ui.label(t!("call.live_sending", file = file.name, size = size))
                            .on_hover_text(t!("call.direct"));
                        action = sending(ui, file, name).or(action.take());
                    }
                    (Some(from), LiveState::Offered) => {
                        ui.label(t!(
                            "call.live_offer",
                            name = name(&from),
                            file = file.name,
                            size = size
                        ))
                        .on_hover_text(t!("call.direct"));
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
                                t!("call.live_saved", file = file.name)
                            } else {
                                t!("call.live_failed", file = file.name)
                            });
                            if ui::small_close(ui, t!("common.dismiss")).clicked() {
                                action = Some(LiveAction::Remove(file.id, false));
                            }
                        });
                    }
                }
            });
    }
    action
}

/// Who receives our live file, and t!("call.stop_sending").
fn sending(ui: &mut Ui, file: &LiveFile, name: impl Fn(&Id) -> String) -> Option<LiveAction> {
    let who: Vec<_> = file.receivers.iter().map(name).collect();
    ui.label(ui::caption(match who.len() {
        0 => t!("call.live_waiting").to_owned(),
        _ => t!("call.live_receiving", names = who.join(", ")),
    }));
    let stop = ui::secondary(ui, t!("call.stop_sending")).clicked();
    stop.then_some(LiveAction::Remove(file.id, true))
}

/// Download an incoming offer, or show progress and Cancel once it starts.
fn receiving(ui: &mut Ui, id: Id, name: &str, size: u64, saved: Option<u64>) -> Option<LiveAction> {
    match saved {
        None => ui
            .add_enabled_ui(files::PICKER, |ui| ui::primary(ui, t!("files.download")))
            .inner
            .clicked()
            .then(|| LiveAction::Save(id, name.to_owned())),
        Some(bytes) => {
            let done = bytes as f32 / size.max(1) as f32;
            ui.add(egui::ProgressBar::new(done).show_percentage());
            ui::secondary(ui, t!("common.cancel"))
                .clicked()
                .then_some(LiveAction::Remove(id, true))
        }
    }
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
        ui::max_width(ui, 360.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("📡").size(ui::TEXT_ICON_LARGE))
                .on_hover_text(t!("call.direct_chat"));
            ui.vertical(|ui| {
                ui.label(ui::bold(name));
                ui.label(ui::caption(t!("call.live_size", size = files::size(*size))))
                    .on_hover_text(t!("call.direct_chat"));
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
                        ui.weak(t!("call.saved"));
                    }
                    (_, Some(LiveState::Failed)) => {
                        ui::error_text(ui, Some(t!("call.didnt_arrive")));
                    }
                    (false, None | Some(LiveState::Offered)) if fresh => {
                        action = receiving(ui, *id, name, *size, None);
                    }
                    (_, Some(LiveState::Ended)) => {
                        ui.weak(t!("call.stopped"));
                    }
                    _ => {
                        ui.weak(t!("call.no_longer_available"));
                    }
                }
            });
        });
    });
    action
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

/// The quality steps of t!("call.share_screen"): height cap, label and hint. The picture
/// keeps its shape and is only ever scaled down, so a smaller screen stays as it is.
fn screen_qualities() -> [(Option<u32>, &'static str, &'static str); 4] {
    [
        (Some(720), "720p", t!("call.quality_720")),
        (Some(1080), "1080p", t!("call.quality_1080")),
        (Some(1440), "1440p", t!("call.quality_1440")),
        (
            None,
            t!("call.quality_native"),
            t!("call.quality_native_help"),
        ),
    ]
}

fn screen_rates() -> [(u32, &'static str, &'static str); 3] {
    [
        (15, "15", t!("call.fps_15")),
        (30, "30", t!("call.fps_30")),
        (60, "60", t!("call.fps_60")),
    ]
}

/// The last choices from Settings, or the defaults for values this version no longer offers.
pub(crate) fn screen_options(s: &Settings) -> Options {
    let default = Options::default();
    let height = screen_qualities().into_iter().map(|q| q.0);
    let fps = screen_rates().into_iter().map(|r| r.0);
    Options {
        max_height: height
            .clone()
            .find(|h| *h == s.screen_height)
            .unwrap_or(default.max_height),
        fps: fps
            .clone()
            .find(|f| *f == s.screen_fps)
            .unwrap_or(default.fps),
        cursor: s.screen_cursor,
        remote_control: false,
    }
}

fn remember_screen_options(s: &mut Settings, options: Options) {
    s.screen_height = options.max_height;
    s.screen_fps = options.fps;
    s.screen_cursor = options.cursor;
}

/// Switch the screen being shared. Closing the picker keeps the current share.
fn change_screen(ctx: &egui::Context, voice: &mut Voice, server: &Server) {
    if voice.sharing != Sharing::On {
        return;
    }
    if cfg!(target_os = "linux") && !rusttalk_core::screen::linux_remote_control() {
        let portal = screen::targets()
            .ok()
            .and_then(|found| found.into_iter().find(|t| t.kind == Kind::Portal));
        if let Some(portal) = portal {
            voice.sharing = Sharing::Changing;
            server.send(Command::ChangeScreen(portal, false));
        }
    } else {
        voice.changing_screen = true;
        open_screen_dialog(ctx, voice);
    }
}

/// Apply what the network says about shared sound. An app stopping on its own never
/// touches the stream's sound: going live stops apps that move into the stream.
pub fn shared_sound(voice: &mut Voice, update: Update) {
    shared_sound_in(&mut voice.shares, &mut voice.stream_audio, update);
}

fn shared_sound_in(shares: &mut Vec<App>, stream: &mut Vec<App>, update: Update) {
    match update {
        Update::AppStopped(id) => shares.retain(|app| app.id != id),
        Update::StreamAudio(apps) => *stream = apps,
        _ => {}
    }
}

/// Open t!("call.share_screen") with the last choices and look for what to share.
fn open_screen_dialog(ctx: &egui::Context, voice: &mut Voice) {
    voice.share_screen = Some((screen_options(&voice.audio.settings()), None));
    if !voice.changing_screen {
        voice.stream_audio = voice.default_stream_audio();
    }
    fill(ctx, &voice.targets, "screens", screen::targets);
}

fn control_option(ui: &mut Ui, enabled: &mut bool) {
    let warning = if cfg!(target_os = "macos") {
        t!("remote.share_warning_mac")
    } else {
        t!("remote.share_warning")
    };
    let changed = ui::toggle_row(ui, enabled, t!("remote.share_opt_in"), warning).changed();
    // macOS drops posted input until RustTalk has the Accessibility permission.
    if *enabled && !rusttalk_core::screen::control_permitted() {
        if changed {
            rusttalk_core::screen::request_control_permission();
        }
        ui::error_text(ui, Some(t!("remote.mac_accessibility")));
    }
}

/// The t!("call.share_screen") dialog: what to share (Windows), quality, frame rate, cursor.
pub fn screen_dialog(ctx: &egui::Context, home: &mut Home) {
    // A moderator's mute also takes a stream's sound.
    let muted = call_server(home).is_some_and(|server| server.voice_muted(&server.dir.me));
    let voice = &mut home.voice;
    if muted {
        voice.stream_audio.clear();
    }
    let Some((options, picked)) = &mut voice.share_screen else {
        return;
    };
    let mut close = false;
    let mut share = None;
    let changing = voice.changing_screen;
    let title = if changing {
        t!("call.change_share")
    } else {
        t!("call.share_screen")
    };
    let ((), dismissed) = ui::dialog(ctx, "share screen", title, |ui| {
        let targets = audio::lock(&voice.targets);
        // Linux lists one entry that opens the system's own picker.
        let portal = match &*targets {
            Listing::Loading => {
                ui.spinner();
                None
            }
            Listing::Failed => {
                ui.weak(t!("call.screens_failed"));
                None
            }
            Listing::Found(found) => {
                let portal = found.iter().position(|t| t.kind == Kind::Portal);
                if portal.is_none() {
                    for (heading, kind) in [
                        (t!("call.screens"), Kind::Monitor),
                        (t!("call.windows"), Kind::Window),
                    ] {
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
        // A change keeps the quality, frame rate and cursor the share started with.
        if !changing {
            section(ui, t!("call.quality"));
            ui.add_space(ui::SPACE_XS);
            ui::segmented(ui, &mut options.max_height, &screen_qualities());
            section(ui, t!("call.frame_rate"));
            ui.add_space(ui::SPACE_XS);
            ui::segmented(ui, &mut options.fps, &screen_rates());
            ui.add_space(ui::SPACE_S);
            ui::toggle_row(
                ui,
                &mut options.cursor,
                t!("call.show_cursor"),
                t!("call.show_cursor_help"),
            );
            section(ui, t!("call.stream_sound"));
            ui.add_space(ui::SPACE_XS);
            let label = format!("🔊  {}", panel::sound_summary(&voice.stream_audio));
            let button = ui
                .add_enabled_ui(voice.supported && !muted, |ui| ui::secondary(ui, &label))
                .inner;
            let button = if !voice.supported {
                button.on_disabled_hover_text(t!("call.needs_windows_11"))
            } else if muted {
                button.on_disabled_hover_text(t!("call.server_muted"))
            } else {
                button
            };
            if button.clicked() {
                fill(ctx, &voice.apps, "apps", share::list);
            }
            let (picked, refresh) = panel::sound_menu(&button, &voice.apps, &voice.stream_audio);
            if let Some(picked) = picked {
                voice.stream_audio = picked;
            }
            if refresh {
                fill(ctx, &voice.apps, "apps", share::list);
            }
        }
        if rusttalk_core::screen::linux_remote_control()
            || rusttalk_core::screen::mac_remote_control()
        {
            control_option(ui, &mut options.remote_control);
        }
        ui::footer(ui, |ui| {
            let label = if portal.is_some() {
                t!("call.choose_share")
            } else if changing {
                t!("call.switch")
            } else {
                t!("call.share")
            };
            let target = match &*targets {
                Listing::Found(found) => portal.or(*picked).and_then(|at| found.get(at)),
                _ => None,
            };
            let button = ui.add_enabled_ui(target.is_some(), |ui| ui::primary(ui, label));
            if button.inner.clicked() {
                share = target.cloned();
            }
            close |= ui::secondary(ui, t!("common.cancel")).clicked();
        });
    });
    if let Some(target) = share {
        let options = *options;
        if changing {
            if let Some(server) = call_server(home) {
                server.send(Command::ChangeScreen(target, options.remote_control));
                home.voice.sharing = Sharing::Changing;
            }
        } else {
            let mut settings = home.voice.audio.settings();
            remember_screen_options(&mut settings, options);
            home.voice.audio.set_settings(settings);
            if let Some(server) = call_server(home) {
                let audio = home.voice.stream_audio.clone();
                server.send(Command::ShareScreen(target, options, audio));
                home.voice.sharing = Sharing::Starting;
            }
        }
        close = true;
    }
    if close || dismissed {
        home.voice.share_screen = None;
        home.voice.changing_screen = false;
    }
}

/// A red t!("call.live_badge") tag for someone sharing their screen.
fn live_badge(ui: &mut Ui) {
    ui::badge(ui, t!("call.live_badge"), ui::colors(ui).danger);
}

/// Settings → Voice & audio.
pub fn settings_tab(ui: &mut Ui, voice: &mut Voice) {
    let audio = voice.audio.clone();
    let mut s = audio.settings();
    let (inputs, outputs) = audio.devices();
    ui::page_section(ui, t!("call.devices"));
    device_picker(ui, t!("call.microphone"), &mut s.input, &inputs);
    device_picker(ui, t!("call.speakers"), &mut s.output, &outputs);
    if let Some(problem) = audio.problem() {
        ui::error_text(ui, Some(&problem));
    }
    mixer::masters(ui, &mut s);
    ui::page_section(ui, t!("call.input_mode"));
    ui.horizontal_wrapped(|ui| {
        let cards = [
            (
                false,
                "🔊",
                t!("call.voice_activity"),
                t!("call.voice_activity_help"),
            ),
            (
                true,
                "⌨",
                t!("call.push_to_talk"),
                t!("call.push_to_talk_help"),
            ),
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
        ui.spacing_mut().slider_width = meter_width(ui);
        if s.auto_threshold {
            // Shows the live automatic level; turning automatic off keeps the person's own.
            let mut live = audio.auto_threshold();
            let slider = ui.add_enabled(
                false,
                Slider::new(&mut live, FLOOR..=CEILING).show_value(false),
            );
            slider.widget_info(|| {
                egui::WidgetInfo::labeled(Role::Slider, false, t!("call.sensitivity"))
            });
            ui.weak(t!("call.sensitivity_help"));
        } else {
            let slider = ui.add(Slider::new(&mut s.threshold, FLOOR..=CEILING).show_value(false));
            slider.widget_info(|| {
                egui::WidgetInfo::labeled(Role::Slider, true, t!("call.sensitivity"))
            });
            ui.weak(t!("call.sensitivity_drag_help"));
        }
        ui::toggle_row(
            ui,
            &mut s.auto_threshold,
            t!("call.auto_threshold"),
            t!("call.auto_threshold_help"),
        );
        if !s.auto_threshold {
            calibrate(ui, voice, &mut s);
        }
    }
    ui::page_section(ui, t!("call.voice_processing"));
    ui::toggle_row(
        ui,
        &mut s.echo_cancellation,
        t!("call.echo"),
        t!("call.echo_help"),
    );
    ui::toggle_row(
        ui,
        &mut s.noise_suppression,
        t!("call.noise"),
        t!("call.noise_help"),
    );
    highlighted(ui, &mut voice.highlight, Row::AiNoise, |ui| {
        ai_noise(ui, &audio, &mut s)
    });
    highlighted(ui, &mut voice.highlight, Row::Gain, |ui| {
        ui::toggle_row(ui, &mut s.auto_gain, t!("call.gain"), t!("call.gain_help"));
    });
    ui::toggle_row(
        ui,
        &mut s.mic_hint,
        t!("call.mic_hint"),
        t!("call.mic_hint_help"),
    );
    audio.set_settings(s);
}

/// Draws `row`; if a hint pointed to it, scrolls it into view and tints it, fading out.
fn highlighted(
    ui: &mut Ui,
    highlight: &mut Option<Highlight>,
    row: Row,
    add: impl FnOnce(&mut Ui),
) {
    let shape = ui.painter().add(egui::Shape::Noop);
    let top = ui.cursor().top();
    add(ui);
    let Some(active) = highlight.as_mut().filter(|h| h.row == row) else {
        return;
    };
    let rect = egui::Rect::from_x_y_ranges(ui.max_rect().x_range(), top..=ui.cursor().top());
    let now = ui.input(|input| input.time);
    let since = *active.since.get_or_insert(now);
    let age = now - since;
    // The first frames only measure the rows (sizes settle a frame later).
    if age < 0.3 {
        ui.scroll_to_rect(rect, Some(egui::Align::Center));
    }
    if age >= HIGHLIGHT_SECONDS {
        *highlight = None;
        return;
    }
    let fade = 1.0 - (age / HIGHLIGHT_SECONDS).powi(2) as f32;
    ui.painter().set(
        shape,
        egui::Shape::rect_filled(
            rect.expand(ui::SPACE_XS),
            ui::RADIUS_ROW,
            ui::accent(ui).base.gamma_multiply(0.25 * fade),
        ),
    );
    ui.ctx().request_repaint();
}

/// The AI noise removal row: download its model first, then an on/off switch.
fn ai_noise(ui: &mut Ui, audio: &Audio, s: &mut Settings) {
    let label = t!("call.ai_noise");
    match audio.model() {
        audio::Model::Ready => {
            ui::toggle_row(ui, &mut s.ai_noise, label, t!("call.ai_noise_help"));
            if s.ai_noise {
                ui::setting_row(ui, t!("call.strength"), t!("call.strength_help"), |ui| {
                    ui.label(t!("call.strength_light"));
                    let slider =
                        ui.add(Slider::new(&mut s.noise_strength, 6..=48).show_value(false));
                    ui.label(t!("call.strength_strong"));
                    slider.widget_info(|| {
                        egui::WidgetInfo::labeled(Role::Slider, true, t!("call.strength"))
                    });
                });
            }
        }
        audio::Model::Downloading(bytes) => {
            ui::setting_row(ui, label, t!("call.downloading"), |ui| {
                let done = bytes as f32 / denoise::SIZE as f32;
                ui.add(
                    egui::ProgressBar::new(done)
                        .desired_width(PICKER_WIDTH)
                        .show_percentage(),
                );
            });
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        model @ (audio::Model::Missing | audio::Model::Failed) => {
            let help = t!(
                "call.ai_noise_download_help",
                size = denoise::SIZE.div_ceil(1_000_000)
            );
            ui::setting_row(ui, label, &help, |ui| {
                let button = ui::secondary(ui, t!("call.download"));
                button.widget_info(|| {
                    egui::WidgetInfo::labeled(Role::Button, true, t!("call.download_ai_noise"))
                });
                if button.clicked() {
                    // It turns on as soon as the download is checked.
                    s.ai_noise = true;
                    audio.download_model();
                }
            });
            if model == audio::Model::Failed {
                ui::error_text(ui, Some(t!("call.download_failed")));
            }
        }
    }
    if s.ai_noise && audio.ai_failed() {
        ui::error_text(ui, Some(t!("call.ai_noise_failed")));
    }
}

fn device_picker(
    ui: &mut Ui,
    label: &str,
    chosen: &mut Option<String>,
    devices: &[audio::DeviceInfo],
) {
    let current = match chosen {
        None => t!("common.system_default"),
        Some(id) => devices
            .iter()
            .find(|d| d.id == *id)
            .map_or(t!("call.not_connected"), |d| d.name.as_str()),
    };
    ui::setting_row(ui, label, "", |ui| {
        let combo = ComboBox::from_id_salt(label)
            .width(PICKER_WIDTH)
            .selected_text(current)
            .show_ui(ui, |ui| {
                ui.selectable_value(chosen, None, t!("common.system_default"));
                for device in devices {
                    ui.selectable_value(chosen, Some(device.id.clone()), &device.name);
                }
            });
        combo
            .response
            .widget_info(|| egui::WidgetInfo::labeled(Role::ComboBox, true, label));
    });
}

/// The meter's width: `METER_WIDTH`, or the page's on a phone.
fn meter_width(ui: &Ui) -> f32 {
    METER_WIDTH.min(ui.available_width())
}

/// The live input level, with the voice activation line.
fn meter(ui: &mut Ui, audio: &Audio, s: &Settings) {
    let fraction = |db: f32| ((db - FLOOR) / (CEILING - FLOOR)).clamp(0.0, 1.0);
    let level = audio.level();
    let width = meter_width(ui);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 10.0), Sense::hover());
    let painter = ui.painter();
    let c = ui::colors(ui);
    painter.rect_filled(rect, ui::RADIUS_S, c.field);
    let threshold = if s.auto_threshold {
        audio.auto_threshold()
    } else {
        s.threshold
    };
    let open = if s.push_to_talk {
        audio.talking()
    } else {
        level > threshold
    };
    let mut filled = rect;
    filled.set_width(rect.width() * fraction(level));
    painter.rect_filled(
        filled,
        ui::RADIUS_S,
        if open { c.success } else { c.offline },
    );
    if !s.push_to_talk {
        let x = rect.left() + rect.width() * fraction(threshold);
        painter.vline(x, rect.y_range(), Stroke::new(2.0, c.strong));
    }
    response.widget_info(|| egui::WidgetInfo::labeled(Role::Meter, true, t!("call.mic_level")));
}

fn key_picker(ui: &mut Ui, voice: &mut Voice, s: &mut Settings) {
    ui.horizontal(|ui| {
        if voice.picking_key {
            ui.label(t!("call.press_key"));
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
            ui.label(t!("call.talk_key", key = s.key));
            if ui::secondary(ui, t!("common.change")).clicked() {
                voice.picking_key = true;
            }
        }
    });
    ui.weak(t!("call.talk_key_help"));
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
                    t!("call.calibrated").into()
                }
                None => t!("call.calibrate_failed").into(),
            }))
        }
        other => other,
    };
    match &voice.calibration {
        Some(Calibration::Quiet(_)) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(t!("call.calibrate_quiet"));
            });
        }
        Some(Calibration::Speak(..)) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(ui::bold(t!("call.calibrate_speak")));
            });
        }
        done => {
            if let Some(Calibration::Done(text)) = done {
                ui.label(text.as_str());
            }
            if ui::secondary(ui, t!("call.calibrate")).clicked() {
                audio.record(true);
                voice.calibration = Some(Calibration::Quiet(now + Duration::from_secs(2)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_tick_is_slow_unless_something_live_shows() {
        let ms = Duration::from_millis;
        // Out of a call with settings closed: no tick at all.
        assert_eq!(tick(false, false, true, None), None);
        // A quiet call: only the slow fallback, focused or not.
        assert_eq!(tick(true, false, true, None), Some(ms(1000)));
        assert_eq!(tick(true, false, false, None), Some(ms(1000)));
        // Someone speaks: draw again when their ring may turn off.
        assert_eq!(tick(true, false, false, Some(ms(240))), Some(ms(240)));
        // The settings meter is live only while the window has focus.
        assert_eq!(tick(true, true, true, None), Some(ms(100)));
        assert_eq!(tick(false, true, false, None), Some(ms(1000)));
        assert_eq!(tick(true, true, true, Some(ms(40))), Some(ms(40)));
    }

    #[test]
    fn going_live_moves_shared_apps_into_the_stream() {
        let spotify = App {
            id: "1:10".into(),
            name: "Spotify".into(),
        };
        // The share dialog folded the shared app into the stream's sound.
        let (mut shares, mut stream) = (vec![spotify.clone()], vec![spotify.clone()]);
        // What going live sends: the app stops on its own, the stream carries it.
        shared_sound_in(
            &mut shares,
            &mut stream,
            Update::AppStopped(spotify.id.clone()),
        );
        assert_eq!(
            stream,
            std::slice::from_ref(&spotify),
            "a stopped app keeps the stream's sound"
        );
        shared_sound_in(
            &mut shares,
            &mut stream,
            Update::StreamAudio(vec![spotify.clone()]),
        );
        assert!(shares.is_empty());
        assert_eq!(stream, [spotify]);
        // A failed or ended stream capture arrives as the whole new selection.
        shared_sound_in(&mut shares, &mut stream, Update::StreamAudio(Vec::new()));
        assert!(stream.is_empty());
    }

    #[test]
    fn screen_choices_are_remembered() {
        assert_eq!(screen_options(&Settings::default()), Options::default());
        for (max_height, _, _) in screen_qualities() {
            for (fps, _, _) in screen_rates() {
                for cursor in [false, true] {
                    let options = Options {
                        max_height,
                        fps,
                        cursor,
                        ..Options::default()
                    };
                    let mut settings = Settings::default();
                    remember_screen_options(&mut settings, options);
                    // Through the settings file and back.
                    let saved = serde_json::to_string(&settings).unwrap();
                    let loaded: Settings = serde_json::from_str(&saved).unwrap();
                    assert_eq!(screen_options(&loaded), options);
                }
            }
        }
        assert!(screen_qualities().into_iter().any(|q| q.0 == Some(1440)));
    }

    #[test]
    fn watching_survives_reconnecting_and_grace_but_expires_on_normal_stop() {
        let now = Instant::now();
        let mut watch = Watching {
            sid: "old".into(),
            texture: None,
            missing_since: None,
            grace_until: None,
        };
        // Normal healthy call: stop-share expires immediately.
        assert!(watch.expired(false, false, now));

        // While reconnecting: retained up to 180 seconds, then expires.
        assert!(!watch.expired(false, true, now));
        assert!(!watch.expired(false, true, now + Duration::from_secs(179)));
        assert!(!watch.expired(true, true, now + Duration::from_secs(180)));
        assert!(!watch.expired(false, true, now + Duration::from_secs(181)));
        assert!(watch.expired(false, true, now + Duration::from_secs(361)));

        // Brief post-recovery grace window retains until deadline, then clears once seen again.
        watch.missing_since = None;
        watch.grace_until = Some(now + Duration::from_secs(15));
        assert!(!watch.expired(false, false, now + Duration::from_secs(14)));
        assert!(watch.expired(false, false, now + Duration::from_secs(15)));
        watch.grace_until = Some(now + Duration::from_secs(15));
        assert!(!watch.expired(true, false, now + Duration::from_secs(5)));
        assert!(watch.grace_until.is_none());
        assert!(watch.expired(false, false, now + Duration::from_secs(6)));
    }

    #[test]
    fn unknown_screen_choices_fall_back() {
        let settings = Settings {
            screen_height: Some(900),
            screen_fps: 24,
            screen_cursor: false,
            ..Settings::default()
        };
        let options = screen_options(&settings);
        assert_eq!(options.max_height, Options::default().max_height);
        assert_eq!(options.fps, Options::default().fps);
        assert!(!options.cursor);
        // An older settings file without these keys gets the defaults.
        let old: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(screen_options(&old), Options::default());
    }
}
