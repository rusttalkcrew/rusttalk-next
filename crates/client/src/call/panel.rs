//! The voice panel at the bottom of the channel list: a header with connection
//! quality and hang-up, one row of share buttons, the "now sharing" card above it
//! and the connection popup.

use std::{collections::VecDeque, sync::Mutex};

use eframe::egui::{
    self, Align, Color32, CursorIcon, Frame, Label, Layout, Margin, Response, RichText, Role,
    Sense, Ui, Vec2, WidgetInfo,
};
use rusttalk_core::client::voice::{CallTraffic, TrackSource, Traffic};

use super::{
    Listing, LiveAction, Row, Sharing, Voice, audio, change_screen as change_screen_now, fill,
    leave, live_files, open_screen_dialog, screen, status_name,
};
use crate::{
    camera::{Controls, Pick},
    files::Ask,
    home::{Home, Tab, user_bar},
    mic_check::Verdict,
    net::Command,
    server::{CallState, Id, Server, Stats},
    share::{self, App},
    terminal::Shell,
    ui::{self, HOVER_SECONDS, ICON_BUTTON, Look, Palette, RADIUS, SPACE_S, SPACE_XS},
};

/// Samples kept for the charts: five minutes at one every two seconds.
pub const HISTORY: usize = 150;
/// Up to these, the connection counts as good, then as fair.
const GOOD_PING_MS: f64 = 150.0;
const FAIR_PING_MS: f64 = 300.0;
const GOOD_LOSS_PERCENT: f64 = 2.0;
const FAIR_LOSS_PERCENT: f64 = 8.0;
const SIGNAL_SIZE: Vec2 = Vec2::splat(ICON_BUTTON);
const CHART_HEIGHT: f32 = 64.0;
const TRAFFIC_CHART_HEIGHT: f32 = 52.0;
const STOP_SIZE: Vec2 = Vec2::splat(24.0);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Quality {
    Unknown,
    Poor,
    Fair,
    Good,
}

/// Quality from the round trip time and the share of incoming packets lost.
pub fn quality(ping_ms: Option<f64>, loss_percent: Option<f64>) -> Quality {
    let Some(ping) = ping_ms else {
        return Quality::Unknown;
    };
    let loss = loss_percent.unwrap_or(0.0);
    if ping <= GOOD_PING_MS && loss < GOOD_LOSS_PERCENT {
        Quality::Good
    } else if ping <= FAIR_PING_MS && loss < FAIR_LOSS_PERCENT {
        Quality::Fair
    } else {
        Quality::Poor
    }
}

impl Quality {
    fn color(self, c: &Palette) -> Color32 {
        match self {
            Quality::Unknown => c.offline,
            Quality::Poor => c.danger_text,
            Quality::Fair => c.warning,
            Quality::Good => c.success,
        }
    }

    /// Signal bars lit, of four.
    fn bars(self) -> usize {
        match self {
            Quality::Unknown => 0,
            Quality::Poor => 1,
            Quality::Fair => 3,
            Quality::Good => 4,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Quality::Unknown => t!("call.measuring"),
            Quality::Poor => t!("call.quality_poor"),
            Quality::Fair => t!("call.quality_okay"),
            Quality::Good => t!("call.quality_good"),
        }
    }
}

/// The call's recent ping, traffic and packet loss, sampled every two seconds.
#[derive(Default)]
pub struct Link {
    pings: VecDeque<f64>,
    traffic_kbps: VecDeque<f64>,
    /// The previous cumulative lost and received counts.
    last: Option<(i64, u64)>,
    /// Incoming packets lost between the last two samples, in percent.
    loss: Option<f64>,
}

impl Link {
    pub fn push(&mut self, stats: &Stats) {
        if self.traffic_kbps.len() == HISTORY {
            self.traffic_kbps.pop_front();
        }
        let [upload, download] = stats.traffic.total();
        self.traffic_kbps
            .push_back((upload.bps + download.bps) / 1000.0);
        if let Some(ping) = stats.ping_ms {
            if self.pings.len() == HISTORY {
                self.pings.pop_front();
            }
            self.pings.push_back(ping);
        }
        let (Some(lost), Some(received)) = (stats.lost_in, stats.received_in) else {
            self.last = None;
            self.loss = None;
            return;
        };
        if let Some((was_lost, was_received)) = self.last {
            // Counters drop when someone leaves; then just start again from here.
            let lost_now = lost - was_lost;
            let received_now = received as i64 - was_received as i64;
            if lost_now >= 0 && received_now >= 0 && lost_now + received_now > 0 {
                self.loss = Some(100.0 * lost_now as f64 / (lost_now + received_now) as f64);
            }
        }
        self.last = Some((lost, received));
    }

    pub fn average(&self) -> Option<f64> {
        let count = self.pings.len();
        (count > 0).then(|| self.pings.iter().sum::<f64>() / count as f64)
    }
}

fn camera_tip(on: bool) -> &'static str {
    if on {
        t!("call.camera_off")
    } else {
        t!("call.camera_on")
    }
}

fn screen_tip(sharing: Sharing) -> &'static str {
    match sharing {
        Sharing::Off => t!("call.share_screen"),
        Sharing::Starting | Sharing::Changing | Sharing::On => t!("call.stop_sharing"),
    }
}

/// What a button on the "now sharing" card does.
enum Stop {
    /// Pick another screen to share.
    Change,
    App(String),
    /// The screen share's sound changed to these.
    Sound(Vec<App>),
}

/// What the person picked on the notice for someone else's phone call.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PhonePick {
    /// Share the microphone with this call, or stop sharing it.
    Toggle,
    /// Share it, and with every later phone call too (Settings → Privacy turns it off).
    Always,
}

fn external_phone_notice(
    ui: &mut Ui,
    label: &str,
    status: crate::phone::ConsentStatus,
) -> Option<PhonePick> {
    ui.label(format!("{}: {label}", t!("phone.external_notice")));
    ui.label(t!("phone.external_help"));
    use crate::phone::ConsentStatus;
    let text = match status {
        ConsentStatus::On => t!("phone.withdraw"),
        ConsentStatus::Off => t!("phone.allow"),
        ConsentStatus::Withdrawing => {
            ui.label(t!("phone.withdrawing"));
            return None;
        }
        ConsentStatus::Failed => {
            ui.label(t!("phone.withdraw_failed"));
            return None;
        }
    };
    let mut pick = None;
    // One under the other: the side panel is too narrow for both in a row.
    if ui::primary(ui, text).clicked() {
        pick = Some(PhonePick::Toggle);
    }
    if status == ConsentStatus::Off && ui::secondary(ui, t!("phone.always_allow")).clicked() {
        pick = Some(PhonePick::Always);
    }
    pick
}

/// What the call lets this person do right now, for the call's buttons.
struct Can {
    connected: bool,
    share_video: bool,
    /// Why talking (and so sharing sound) is off, if it is.
    blocked: Option<&'static str>,
}

impl Can {
    fn of(home: &Home) -> Option<Self> {
        let call = home.voice.call.as_ref()?;
        let server = home.servers.iter().find(|s| s.address == call.server);
        // A moderator's mute also takes shared app audio.
        let blocked = if server.is_some_and(|server| server.voice_muted(&server.dir.me)) {
            Some(t!("call.server_muted"))
        } else if !call.can_talk {
            Some(t!("call.no_talk_here"))
        } else {
            None
        };
        Some(Self {
            connected: call.state == CallState::Connected,
            share_video: call.can_share_video,
            blocked,
        })
    }
}

/// What the call's buttons asked for this frame; `apply` carries it out once the
/// buttons no longer borrow the call.
#[derive(Default)]
struct Picked {
    leave: bool,
    share: Vec<(App, bool)>,
    stopped: Option<Stop>,
    stop_screen: bool,
    change_screen: bool,
    terminal: Option<Shell>,
    diagram: Option<super::diagram::Pick>,
    live: Option<LiveAction>,
    voice_settings: bool,
    video_settings: bool,
}

/// Which optional buttons the share row has on this device.
struct Extras {
    screen: bool,
    app_audio: bool,
    terminal: bool,
    phone: bool,
}

impl Extras {
    fn of(voice: &mut Voice, app_audio: bool) -> Self {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let phone = super::phone::shows(voice);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let phone = false;
        Self {
            screen: screen::SUPPORTED,
            app_audio,
            terminal: voice.audio.can_host_terminal(),
            phone,
        }
    }

    fn count(&self) -> usize {
        // The camera and "More" (diagrams), plus whatever else this device can share.
        2 + usize::from(self.screen) + usize::from(self.app_audio)
    }
}

/// `count` buttons sharing the row's width evenly, `height` tall.
fn even(ui: &Ui, count: usize, height: f32) -> Vec2 {
    let count = count as f32;
    let width = (ui.available_width() - SPACE_XS * (count - 1.0)) / count;
    Vec2::new(width, height)
}

/// The voice panel, while in a voice room.
pub fn call_bar(ui: &mut Ui, home: &mut Home) {
    if home.voice.call.is_none() {
        return;
    }
    let mut withdrawing = Vec::new();
    for (call, label, allowed) in &home.voice.external_phones {
        ui.push_id(&call.track, |ui| {
            if let Some(pick) = external_phone_notice(ui, label, *allowed)
                && let Some(server) = super::call_server(home)
            {
                if pick == PhonePick::Always {
                    let mut settings = home.voice.audio.settings();
                    settings.phone_always_share = true;
                    home.voice.audio.set_settings(settings);
                }
                let on = *allowed == crate::phone::ConsentStatus::Off;
                if !on {
                    home.voice.audio.request_phone_withdrawal();
                    withdrawing.push(call.clone());
                }
                server.send(Command::PhoneConsent(call.clone(), on));
            }
        });
    }
    for (call, _, status) in &mut home.voice.external_phones {
        if withdrawing.contains(call) {
            *status = crate::phone::ConsentStatus::Withdrawing;
        }
    }
    mic_hint_card(ui, home);
    let Some(can) = Can::of(home) else {
        return;
    };
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
    let c = ui::colors(ui);
    let (text, color) = match call.state {
        CallState::Connecting => (t!("common.connecting"), c.warning),
        CallState::Connected => (t!("call.voice_connected"), c.success),
        CallState::Reconnecting => (t!("call.reconnecting"), c.warning),
    };
    let stats = call.stats.clone();
    let channel = call.channel;
    let quality = if can.connected {
        quality(stats.ping_ms, home.voice.link.loss)
    } else {
        Quality::Unknown
    };
    let mut picked = Picked {
        stopped: sharing_card(ui, &home.voice, can.blocked),
        ..Picked::default()
    };
    Frame::new()
        .fill(c.field)
        .corner_radius(RADIUS)
        .inner_margin(Margin::same(ui::SPACE_S as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui::right_row(ui, ICON_BUTTON, |ui| {
                picked.leave = leave_button(ui, SIGNAL_SIZE).clicked();
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let signal = signal_button(ui, quality);
                    ui::popup(&signal, |ui| {
                        picked.voice_settings =
                            connection(ui, &stats, &home.voice.link, quality, server, &channel);
                    });
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add(Label::new(ui::bold(text).color(color)).truncate());
                        ui.add(Label::new(ui::caption(&place)).truncate());
                    });
                });
            });
            ui.add_space(6.0);
            let extras = Extras::of(&mut home.voice, true);
            let size = even(ui, extras.count(), ICON_BUTTON);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = SPACE_XS;
                share_buttons(ui, &mut home.voice, &can, &extras, size, &mut picked);
            });
            picked.live = live_files(ui, &home.voice.live_files, server);
        });
    apply(ui, home, picked);
}

/// The call's controls along the bottom of the call view on phones, where the sidebar
/// and its voice panel are hidden: mic, sound, the share buttons and hang up, sized
/// for a finger.
pub fn phone_controls(ui: &mut Ui, home: &mut Home) {
    let Some(can) = Can::of(home) else {
        return;
    };
    let mut picked = Picked::default();
    // Without app audio support the button could only say so; leave room for the rest.
    let app_audio = home.voice.supported;
    let extras = Extras::of(&mut home.voice, app_audio);
    // Mic, sound and hang up around the share buttons.
    let size = even(ui, 3 + extras.count(), ui::TOUCH_TARGET);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = SPACE_XS;
        user_bar::mic_button(ui, home, size);
        user_bar::deafen_button(ui, home, size);
        share_buttons(ui, &mut home.voice, &can, &extras, size, &mut picked);
        picked.leave = leave_button(ui, size).clicked();
    });
    apply(ui, home, picked);
}

/// How many buttons `phone_controls` shows, to size a row for them elsewhere.
pub fn phone_control_count(home: &mut Home) -> usize {
    let app_audio = home.voice.supported;
    3 + Extras::of(&mut home.voice, app_audio).count()
}

/// A right-click on the mic (`mic`) or sound button: pick the device, or open the
/// voice settings.
pub fn device_choices(button: &Response, home: &mut Home, mic: bool) {
    let audio = home.voice.audio.clone();
    let (inputs, outputs) = audio.devices();
    let mut s = audio.settings();
    let mut picked = Picked::default();
    let (title, chosen, devices) = if mic {
        (t!("call.microphone"), &s.input, &inputs)
    } else {
        (t!("call.speakers"), &s.output, &outputs)
    };
    let pick = ui::context_menu(button, |m| {
        device_menu(m, title, chosen, devices, &mut picked)
    });
    if let Some(Some(pick)) = pick {
        if mic {
            s.input = pick;
        } else {
            s.output = pick;
        }
        audio.set_settings(s);
    }
    if picked.voice_settings {
        home.settings = Some(Tab::Voice);
    }
}

/// A device list for a right-click menu: the system default, then each device, with a
/// tick on `chosen`. Returns the pick; voice settings sit at the bottom.
fn device_menu(
    m: &mut ui::Menu,
    title: &str,
    chosen: &Option<String>,
    devices: &[crate::audio::DeviceInfo],
    picked: &mut Picked,
) -> Option<Option<String>> {
    m.caption(title);
    let mut pick = None;
    let default = ui::Item::new("", t!("common.system_default")).picked(chosen.is_none());
    if m.add(default).clicked() {
        pick = Some(None);
    }
    for device in devices {
        let on = chosen.as_ref() == Some(&device.id);
        if m.add(ui::Item::new("", &device.name).picked(on)).clicked() {
            pick = Some(Some(device.id.clone()));
        }
    }
    m.separator();
    if m.item("⚙", t!("call.voice_settings")).clicked() {
        picked.voice_settings = true;
    }
    pick
}

/// The cameras for the camera button's right-click menu, with a tick on `chosen`.
fn camera_choices(
    m: &mut ui::Menu,
    chosen: &Option<String>,
    found: &[String],
) -> Option<Option<String>> {
    m.caption(t!("camera.camera"));
    let mut pick = None;
    let first = ui::Item::new("", t!("camera.first")).picked(chosen.is_none());
    if m.add(first).clicked() {
        pick = Some(None);
    }
    for name in found {
        let label = crate::camera::camera_label(name);
        let on = chosen.as_ref() == Some(name);
        if m.add(ui::Item::new("", &label).picked(on)).clicked() {
            pick = Some(Some(name.clone()));
        }
    }
    pick
}

fn leave_button(ui: &mut Ui, size: Vec2) -> Response {
    ui::icon_button(ui, "📞", t!("call.leave"), Look::Danger, size)
}

/// Camera on or off. On phones, while it is on, a tap opens a menu to flip the
/// camera, switch its light or turn it off. Returned so a caller can hang more
/// choices off it.
fn camera_button(
    ui: &mut Ui,
    voice: &mut Voice,
    can: &Can,
    size: Vec2,
    picked: &mut Picked,
) -> Response {
    let camera = &mut voice.camera;
    let controls = camera.controls();
    let look = if camera.on {
        Look::On(ui::colors(ui).success)
    } else {
        Look::Plain
    };
    let tip = if controls.is_some() {
        t!("camera.camera")
    } else {
        camera_tip(camera.on)
    };
    let button = ui
        .add_enabled_ui(can.connected && can.share_video, |ui| {
            ui::icon_button(ui, "📷", tip, look, size)
        })
        .inner;
    if !can.share_video {
        return button.on_disabled_hover_text(t!("call.no_video_here"));
    }
    let Some(controls) = controls else {
        if button.clicked() {
            camera.on ^= true;
        }
        camera_choices_menu(&button, voice, picked);
        return button;
    };
    if let Some(pick) = ui::menu(&button, |m| camera_menu(m, controls)).flatten() {
        let mut settings = voice.audio.settings();
        voice.camera.pick(pick, &mut settings);
        voice.audio.set_settings(settings);
    }
    button
}

/// A right-click on the camera button: pick the camera, or open the video settings.
/// Cameras are looked for only once the menu opens.
fn camera_choices_menu(button: &Response, voice: &mut Voice, picked: &mut Picked) {
    let found = if ui::menu_open(button) {
        voice.camera.found(&button.ctx)
    } else {
        Vec::new()
    };
    let mut s = voice.audio.settings();
    let pick = ui::context_menu(button, |m| {
        let pick = camera_choices(m, &s.camera, &found);
        m.separator();
        picked.video_settings |= m.item("⚙", t!("call.video_settings")).clicked();
        pick
    });
    if let Some(Some(pick)) = pick {
        s.camera = pick;
        voice.audio.set_settings(s);
    }
}

/// The camera button's menu while the camera is on.
fn camera_menu(m: &mut ui::Menu, controls: Controls) -> Option<Pick> {
    let mut pick = None;
    if controls.flip
        && m.add(ui::Item::new("🔄", t!("camera.flip")).enabled(!controls.busy))
            .clicked()
    {
        pick = Some(Pick::Flip);
    }
    if let Some(on) = controls.torch {
        let label = if on {
            t!("camera.torch_off")
        } else {
            t!("camera.torch_on")
        };
        if m.add(ui::Item::new("💡", label).enabled(!controls.busy))
            .clicked()
        {
            pick = Some(Pick::Torch(!on));
        }
    }
    if m.danger("⏹", t!("call.camera_off")).clicked() {
        pick = Some(Pick::Stop);
    }
    pick
}

/// Camera, screen, app audio and "More", as far as `extras` has them.
fn share_buttons(
    ui: &mut Ui,
    voice: &mut Voice,
    can: &Can,
    extras: &Extras,
    size: Vec2,
    picked: &mut Picked,
) {
    camera_button(ui, voice, can, size, picked);
    if extras.screen {
        screen_button(ui, voice, can, size, picked);
    }
    let talk = can.connected && can.blocked.is_none();
    if extras.app_audio {
        picked.share = app_button(ui, voice, talk, can.blocked, size);
    }
    let can_call = extras.phone && talk;
    let more = more_button(ui, voice, extras.terminal, can_call, can.connected, size);
    (picked.terminal, picked.diagram) = more;
}

/// Share the screen, or stop sharing; right-click while sharing to change or stop.
fn screen_button(
    ui: &mut Ui,
    voice: &mut Voice,
    can: &Can,
    size: Vec2,
    picked: &mut Picked,
) -> Response {
    let look = if voice.sharing == Sharing::Off {
        Look::Plain
    } else {
        Look::On(ui::colors(ui).success)
    };
    let button = ui
        .add_enabled_ui(can.connected && can.share_video, |ui| {
            ui::icon_button(ui, "🖥", screen_tip(voice.sharing), look, size)
        })
        .inner;
    if voice.sharing == Sharing::Off && can.connected && can.share_video {
        let mut s = voice.audio.settings();
        let before = (s.screen_height, s.screen_fps, s.screen_cursor);
        let share = ui::context_menu(&button, |m| screen_quick_options(m, &mut s));
        if (s.screen_height, s.screen_fps, s.screen_cursor) != before {
            voice.audio.set_settings(s);
        }
        if share == Some(true) {
            open_screen_dialog(ui.ctx(), voice);
        }
    }
    if voice.sharing == Sharing::On {
        ui::context_menu(&button, |m| {
            if m.item("🖥", t!("call.change_my_share")).clicked() {
                picked.change_screen = true;
            }
            if m.danger("⏹", t!("call.stop_sharing")).clicked() {
                picked.stop_screen = true;
            }
        });
    }
    if !can.share_video {
        return button.on_disabled_hover_text(t!("call.no_screen_here"));
    }
    if button.clicked() {
        if voice.sharing == Sharing::Off {
            open_screen_dialog(ui.ctx(), voice);
        } else {
            picked.stop_screen = true;
        }
    }
    button
}

/// A right-click on screen share before sharing: start, or quickly set the quality,
/// frame rate and cursor the next share starts with (the share dialog's choices).
/// Returns whether to open the share dialog.
fn screen_quick_options(m: &mut ui::Menu, s: &mut crate::audio::Settings) -> bool {
    let share = m.item("🖥", t!("call.share_screen")).clicked();
    let options = super::screen_options(s);
    m.separator();
    m.caption(t!("call.quality"));
    for (height, label, _) in super::screen_qualities() {
        let item = ui::Item::new("", label).picked(options.max_height == height);
        if m.add(item).clicked() {
            s.screen_height = height;
        }
    }
    m.separator();
    m.caption(t!("call.frame_rate"));
    for (fps, label, _) in super::screen_rates() {
        let item = ui::Item::new("", label).picked(options.fps == fps);
        if m.add(item).clicked() {
            s.screen_fps = fps;
        }
    }
    m.separator();
    let cursor = ui::Item::new("", t!("call.show_cursor")).checked(s.screen_cursor);
    if m.add(cursor).clicked() {
        s.screen_cursor ^= true;
    }
    share
}

/// Carry out what the call's buttons asked for.
fn apply(ui: &Ui, home: &mut Home, picked: Picked) {
    let server = home
        .voice
        .call
        .as_ref()
        .and_then(|call| home.servers.iter().find(|s| s.address == call.server));
    if let Some(Stop::Sound(apps)) = &picked.stopped
        && let Some(server) = server
    {
        home.voice.stream_audio = apps.clone();
        server.send(Command::StreamAudio(apps.clone()));
    }
    for (app, on) in picked.share {
        let Some(server) = server else { break };
        if on {
            home.voice.shares.push(app.clone());
            server.send(Command::ShareApp(app));
        } else {
            home.voice.shares.retain(|shared| shared.id != app.id);
            server.send(Command::StopApp(app.id.clone()));
        }
    }
    let change_screen = picked.change_screen || matches!(picked.stopped, Some(Stop::Change));
    if let (Some(server), Some(Stop::App(id))) = (server, picked.stopped) {
        home.voice.shares.retain(|shared| shared.id != id);
        server.send(Command::StopApp(id));
    }
    if let (Some(server), true) = (server, change_screen) {
        change_screen_now(ui.ctx(), &mut home.voice, server);
    }
    if let (Some(server), true) = (server, picked.stop_screen) {
        home.voice.sharing = Sharing::Off;
        home.voice.stop_preview();
        server.send(Command::StopScreen);
    }
    if let Some(server) = server {
        let address = server.address.clone();
        match picked.live {
            Some(LiveAction::Save(id, name)) => {
                home.files.ask(ui.ctx(), Ask::Accept(address, id, name))
            }
            Some(LiveAction::Remove(id, stop)) => {
                if stop {
                    server.send(Command::CancelFile(id));
                }
                home.voice.live_files.retain(|file| file.id != id);
            }
            None => {}
        }
    }
    if let Some(shell) = picked.terminal {
        super::terminal::toggle(ui.ctx(), home, shell);
    }
    if let Some(pick) = picked.diagram {
        super::diagram::apply(ui.ctx(), home, pick);
    }
    if picked.voice_settings {
        home.settings = Some(Tab::Voice);
    }
    if picked.video_settings {
        home.settings = Some(Tab::Video);
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if picked.leave && super::phone::leave_or_ask(home, super::phone::Leave::Leave) {
        return;
    }
    if picked.leave {
        leave(home);
    }
}

/// The "More" button: a menu for the less common shares, so the row stays short. Starts or
/// stops a terminal share (on Windows also Command Prompt) and a diagram, and calls a
/// phone when an account is set up. Lights up while a phone call runs.
fn more_button(
    ui: &mut Ui,
    voice: &mut Voice,
    terminal: bool,
    can_call: bool,
    connected: bool,
    size: Vec2,
) -> (Option<Shell>, Option<super::diagram::Pick>) {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let calling = super::phone::calling(voice);
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let calling = false;
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let _ = can_call;
    let look = if calling {
        Look::On(ui::colors(ui).success)
    } else {
        Look::Plain
    };
    let on = voice.terminals.local.is_some();
    let button = ui
        .add_enabled_ui(connected || calling, |ui| {
            ui::icon_button(ui, "…", t!("call.more"), look, size)
        })
        .inner;
    let mut picked = None;
    let mut diagram = None;
    ui::menu_wide(&button, |m| {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if can_call || calling {
            super::phone::menu_rows(m, &mut voice.phone, can_call);
            m.separator();
        }
        // Available even where hosting a terminal is not.
        diagram = super::diagram::menu_rows(m, &voice.diagrams);
        if !terminal {
            return;
        }
        m.separator();
        if on {
            if m.danger("⏹", t!("call.stop_terminal")).clicked() {
                picked = Some(Shell::Default);
            }
            return;
        }
        if m.item(crate::terminal::ICON, t!("call.share_terminal"))
            .clicked()
        {
            picked = Some(Shell::Default);
        }
        if cfg!(windows)
            && m.item(crate::terminal::ICON, t!("call.share_cmd"))
                .clicked()
        {
            picked = Some(Shell::Cmd);
        }
    });
    (picked, diagram)
}

/// A gentle hint above the panel when the microphone is noisy or too loud. Never while
/// muted; it waits, and answering it (or leaving the call) is final for this call.
fn mic_hint_card(ui: &mut Ui, home: &mut Home) {
    let audio = home.voice.audio.clone();
    let mut settings = audio.settings();
    let verdict = home
        .voice
        .mic_hint
        .filter(|_| !audio.muted() && !settings.ai_noise);
    let Some(last) = verdict else { return };
    let mut answer = None;
    ui::fade_card(ui, "mic hint", true, |ui| {
        let text = match last {
            Verdict::Noisy => t!("call.mic_noisy"),
            Verdict::Loud => t!("call.mic_loud"),
        };
        ui.add(Label::new(text).wrap());
        ui.add_space(SPACE_XS);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = SPACE_XS;
            if last == Verdict::Noisy && ui::primary(ui, t!("call.mic_turn_on")).clicked() {
                answer = Some(Hint::TurnOn);
            }
            if ui::secondary(ui, t!("call.mic_settings")).clicked() {
                answer = Some(Hint::Settings);
            }
        });
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = SPACE_XS;
            if ui::secondary(ui, t!("call.mic_not_now")).clicked() {
                answer = Some(Hint::NotNow);
            }
            if ui.link(t!("call.mic_dont_ask")).clicked() {
                answer = Some(Hint::Never);
            }
        });
    });
    ui.add_space(4.0);
    let Some(answer) = answer else { return };
    home.voice.mic_hint = None;
    match answer {
        Hint::TurnOn => {
            // Like the Download button in Settings: it starts as soon as the model is ready.
            settings.ai_noise = true;
            audio.download_model();
            audio.set_settings(settings);
        }
        Hint::Settings => {
            home.voice.show_row(match last {
                Verdict::Noisy => Row::AiNoise,
                Verdict::Loud => Row::Gain,
            });
            home.settings = Some(Tab::Voice);
        }
        Hint::NotNow => {}
        Hint::Never => {
            settings.mic_hint = false;
            audio.set_settings(settings);
        }
    }
}

enum Hint {
    TurnOn,
    Settings,
    NotNow,
    Never,
}

/// Four signal bars in the quality's colour; opens the connection popup.
fn signal_button(ui: &mut Ui, quality: Quality) -> Response {
    let (rect, response) = ui.allocate_exact_size(SIGNAL_SIZE, Sense::click());
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered(),
        HOVER_SECONDS,
    );
    let c = ui::colors(ui);
    let fill = Color32::TRANSPARENT.lerp_to_gamma(c.hover, hover);
    let painter = ui.painter();
    painter.rect_filled(rect, RADIUS, fill);
    let (bar, gap, tall) = (3.0, 2.0, 14.0);
    let left = rect.center().x - (4.0 * bar + 3.0 * gap) / 2.0;
    let bottom = rect.center().y + tall / 2.0;
    for at in 0..4 {
        let height = tall * (at + 1) as f32 / 4.0;
        let x = left + at as f32 * (bar + gap);
        let color = if at < quality.bars() {
            quality.color(c)
        } else {
            c.offline.gamma_multiply(0.45)
        };
        let bar =
            egui::Rect::from_min_max(egui::pos2(x, bottom - height), egui::pos2(x + bar, bottom));
        painter.rect_filled(bar, 1, color);
    }
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("call.connection_info")));
    response
        .on_hover_text(t!(
            "call.connection_info_quality",
            quality = quality.label()
        ))
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// The connection popup. True when "Voice settings" was clicked.
fn connection(
    ui: &mut Ui,
    stats: &Stats,
    link: &Link,
    quality: Quality,
    server: Option<&Server>,
    channel: &Id,
) -> bool {
    ui.set_max_height(ui.ctx().content_rect().height() - 2.0 * ui::SPACE_L);
    egui::ScrollArea::vertical()
        .show(ui, |ui| {
            connection_contents(ui, stats, link, quality, server, channel)
        })
        .inner
}

fn connection_contents(
    ui: &mut Ui,
    stats: &Stats,
    link: &Link,
    quality: Quality,
    server: Option<&Server>,
    channel: &Id,
) -> bool {
    let c = ui::colors(ui);
    let color = quality.color(c);
    ui.spacing_mut().item_spacing.y = SPACE_XS;
    ui.horizontal(|ui| {
        ui.label(ui::title(t!("call.connection")));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            quality_pill(ui, quality);
        });
    });
    if let Some(kbps) = server
        .and_then(|server| server.channel(channel))
        .filter(|channel| channel.voice && channel.audio_kbps > 0)
        .map(|channel| channel.audio_kbps)
    {
        ui.label(ui::caption(format!(
            "🎵  {} · {kbps} kbps",
            t!("home.audio_quality_label")
        )));
    }
    ui.add_space(SPACE_S);
    card(ui, |ui| {
        let loss = link
            .loss
            .map_or("—".into(), |v| format!("{} %", crate::i18n::number(v, 1)));
        let tiles = [
            (t!("call.ping"), ms(stats.ping_ms)),
            (t!("call.average"), ms(link.average())),
            (t!("call.packet_loss"), loss),
        ];
        ui.columns(tiles.len(), |columns| {
            for (ui, (label, value)) in columns.iter_mut().zip(tiles) {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add(Label::new(ui::title(value).color(c.strong)).truncate());
                ui.add(Label::new(ui::caption(label)).truncate());
            }
        });
        ui.add_space(SPACE_S);
        let pings: Vec<f64> = link.pings.iter().copied().collect();
        let size = Vec2::new(ui.available_width(), CHART_HEIGHT);
        ui::line_chart(ui, &pings, HISTORY, size, color, 50.0, |v| ms(Some(v)));
        time_axis(ui);
    });
    traffic(ui, &stats.traffic, link);
    ui.add_space(SPACE_XS);
    if let Some(server) = server
        && let Some(peers) = server.voice.get(channel)
        && peers.iter().any(|peer| peer.person != server.dir.me)
    {
        let others = peers.iter().filter(|peer| peer.person != server.dir.me);
        let title = format!("{} · {}", t!("call.people_in_call"), others.clone().count());
        egui::CollapsingHeader::new(title)
            .id_salt("people in call")
            .show(ui, |ui| {
                for peer in others {
                    let muted = server.voice_muted(&peer.person);
                    ui.horizontal(|ui| status_name(ui, &peer.name(server), peer.status, muted));
                }
            });
    }
    egui::CollapsingHeader::new(t!("common.details"))
        .id_salt("connection details")
        .show(ui, |ui| card(ui, |ui| details(ui, stats, server)));
    ui.separator();
    ui.add(Label::new(ui::caption(format!("🔒  {}", t!("call.e2ee")))).wrap())
        .on_hover_text(t!("call.e2ee_help"));
    ui.add_space(SPACE_XS);
    ui.vertical_centered_justified(|ui| {
        ui::secondary(ui, &format!("⚙  {}", t!("call.voice_settings"))).clicked()
    })
    .inner
}

fn ms(value: Option<f64>) -> String {
    value.map_or("—".into(), |v| format!("{v:.0} ms"))
}

/// A bit rate as people read it: kbps, or Mbps from one million bits a second.
fn rate(bps: f64) -> String {
    if bps >= 1_000_000.0 {
        format!("{} Mbps", crate::i18n::number(bps / 1_000_000.0, 1))
    } else {
        format!("{:.0} kbps", bps / 1000.0)
    }
}

/// The popup's group of related numbers, set apart on a softer surface.
fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui::card_frame(ui)
        .inner_margin(Margin::same(ui::SPACE_M as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// The quality in its colour, as a soft pill with a dot.
fn quality_pill(ui: &mut Ui, quality: Quality) {
    let color = quality.color(ui::colors(ui));
    Frame::new()
        .fill(color.gamma_multiply(0.15))
        .corner_radius(RADIUS)
        .inner_margin(Margin::symmetric(ui::SPACE_S as i8, 2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = SPACE_XS + 2.0;
            ui::dot(ui, color);
            ui.label(ui::bold(quality.label()).color(color));
        });
}

/// "5 min ago" and "now" under a chart's left and right ends.
fn time_axis(ui: &mut Ui) {
    ui.horizontal(|ui| {
        ui.label(ui::caption(t!("call.five_min_ago")));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(ui::caption(t!("call.now")));
        });
    });
}

/// One row of the traffic table: `label`, then `cells` right-aligned in two equal
/// columns over two thirds of the width, so numbers line up from row to row.
fn table_row(ui: &mut Ui, label: RichText, cells: Vec<Vec<RichText>>) {
    let width = ui.available_width();
    let column = width / 3.0;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        let first = width - column * cells.len() as f32;
        ui.allocate_ui_with_layout(Vec2::new(first, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(first);
            ui.add(Label::new(label).truncate());
        });
        for lines in cells {
            let layout = Layout::top_down(Align::Max);
            ui.allocate_ui_with_layout(Vec2::new(column, 0.0), layout, |ui| {
                ui.set_width(column);
                for line in lines {
                    ui.add(Label::new(line).truncate());
                }
            });
        }
    });
}

/// Upload and download per kind of track, then the total over the last minutes.
/// Kinds that sent nothing either way are left out.
fn traffic(ui: &mut Ui, traffic: &CallTraffic, link: &Link) {
    let c = ui::colors(ui);
    ui::section(ui, t!("call.traffic"));
    card(ui, |ui| {
        let head = |text| vec![ui::caption(text)];
        table_row(
            ui,
            RichText::new(""),
            vec![head(t!("call.upload")), head(t!("call.download_traffic"))],
        );
        ui.add_space(SPACE_XS);
        let row = |ui: &mut Ui, label: &str, pair: [Traffic; 2]| {
            let cells = pair
                .iter()
                .map(|value| {
                    let mb = value.bytes as f64 / 1_000_000.0;
                    vec![
                        RichText::new(rate(value.bps)).color(c.strong),
                        ui::caption(format!("{} MB", crate::i18n::number(mb, 2))),
                    ]
                })
                .collect();
            table_row(ui, RichText::new(label).color(c.text), cells);
            ui.add_space(SPACE_XS);
        };
        row(ui, t!("call.total"), traffic.total());
        for (label, source) in [
            (t!("call.track_voice"), TrackSource::Microphone),
            (t!("call.track_camera"), TrackSource::Camera),
            (t!("call.track_screen"), TrackSource::Screenshare),
            (t!("call.track_app_audio"), TrackSource::ScreenshareAudio),
        ] {
            let pair = traffic.for_source(source);
            if pair.iter().any(|value| value.bytes > 0 || value.bps > 0.0) {
                row(ui, label, pair);
            }
        }
        ui.add_space(SPACE_XS);
        ui.label(ui::caption(t!("call.traffic_caption")));
        let rates: Vec<_> = link.traffic_kbps.iter().copied().collect();
        let size = Vec2::new(ui.available_width(), TRAFFIC_CHART_HEIGHT);
        let color = ui::accent(ui).text(c.dark);
        ui::line_chart(ui, &rates, HISTORY, size, color, 50.0, |kbps| {
            rate(kbps * 1000.0)
        });
        time_axis(ui);
    });
}

/// Jitter, lost packets and bitrates: what the connection window used to show.
fn details(ui: &mut Ui, stats: &Stats, server: Option<&Server>) {
    let count = |value: Option<i64>| value.map_or("—".into(), |v| v.to_string());
    let c = ui::colors(ui);
    // The name takes up to half the width; the value fills the rest from the right
    // and wraps when long, like a video's size, frame rate and bit rate.
    let row = |ui: &mut Ui, label: String, value: String| {
        ui.horizontal_top(|ui| {
            let width = ui.available_width();
            ui.allocate_ui(Vec2::new(width / 2.0, ui::TEXT_SMALL), |ui| {
                let label = RichText::new(label).size(ui::TEXT_SMALL).color(c.text);
                ui.add(Label::new(label).truncate());
            });
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                let value = RichText::new(value).size(ui::TEXT_SMALL).color(c.strong);
                ui.add(Label::new(value).wrap());
            });
        });
    };
    row(ui, t!("call.jitter").into(), ms(stats.jitter_ms));
    row(ui, t!("call.lost_in").into(), count(stats.lost_in));
    row(ui, t!("call.lost_out").into(), count(stats.lost_out));
    for track in &stats.tracks {
        let name = match (track.mine, server) {
            (true, _) => t!("common.you").to_owned(),
            (false, Some(server)) => server.member_name(&track.person),
            (false, None) => String::new(),
        };
        let name = match &track.app {
            Some(app) => format!("{name} · {app}"),
            None => name,
        };
        let kbps = format!("{:.0} kbps", track.kbps);
        let value = match track.video {
            // What a video really runs at: a share set to 60 fps may arrive slower.
            Some(v) => format!("{}×{} · {:.0} fps · {kbps}", v.width, v.height, v.fps),
            None => kbps,
        };
        row(ui, name, value);
    }
}

/// The app audio button and its picker: each app playing sound with a check, a
/// refresh and t!("call.stop_sharing_all"). Returns the apps turned on or off.
fn app_button(
    ui: &mut Ui,
    voice: &mut Voice,
    enabled: bool,
    blocked: Option<&str>,
    size: Vec2,
) -> Vec<(App, bool)> {
    let look = if voice.shares.is_empty() {
        Look::Plain
    } else {
        Look::On(ui::colors(ui).success)
    };
    // Live, the stream carries the sound: pick it under t!("call.now_sharing").
    let live = voice.sharing != Sharing::Off;
    let usable = voice.supported && enabled && !live;
    let button = ui
        .add_enabled_ui(usable, |ui| {
            ui::icon_button(ui, "🎵", t!("call.share_app_audio"), look, size)
        })
        .inner;
    let button = if !voice.supported {
        button.on_disabled_hover_text(t!("call.needs_windows_11"))
    } else if let Some(why) = blocked {
        button.on_disabled_hover_text(why)
    } else if live {
        button.on_disabled_hover_text(t!("call.sound_in_stream"))
    } else {
        button
    };
    // Listing starts a helper process on Linux; keep it off the UI thread.
    let list = |ui: &Ui| fill(ui.ctx(), &voice.apps, "apps", share::list);
    if button.clicked() {
        list(ui);
    }
    let mut changes = Vec::new();
    let mut refresh = false;
    ui::menu_above(&button, |m| {
        m.caption(t!("call.share_app_audio"));
        let (picked, again) = sound_rows(m, &voice.apps, &voice.shares, false);
        refresh = again;
        if let Some(picked) = picked {
            changes = diff(&voice.shares, &picked);
        }
        if !voice.shares.is_empty() && m.danger("⏹", t!("call.stop_sharing_all")).clicked() {
            changes.extend(voice.shares.iter().map(|app| (app.clone(), false)));
        }
    });
    if refresh {
        list(ui);
    }
    changes
}

/// The screen share's sound picker under `button`: t!("call.no_sound"), all system
/// audio or apps. Returns the new pick and whether to list the apps again.
pub(super) fn sound_menu(
    button: &Response,
    apps: &Mutex<Listing<App>>,
    chosen: &[App],
) -> (Option<Vec<App>>, bool) {
    ui::menu_above(button, |m| {
        m.caption(t!("call.stream_sound"));
        sound_rows(m, apps, chosen, true)
    })
    .unwrap_or_default()
}

/// t!("call.all_system_audio") and each app playing now, with a check on the `chosen`
/// ones, then a refresh. `none` adds t!("call.no_sound") on top.
fn sound_rows(
    m: &mut ui::Menu,
    apps: &Mutex<Listing<App>>,
    chosen: &[App],
    none: bool,
) -> (Option<Vec<App>>, bool) {
    let mut picked = None;
    if none
        && m.add(ui::Item::new("🔇", t!("call.no_sound")).picked(chosen.is_empty()))
            .clicked()
    {
        picked = Some(Vec::new());
    }
    let system = share::system();
    let on = chosen.iter().any(share::is_system);
    let item = ui::Item::new("🔊", &system.name)
        .hint(t!("call.all_system_audio_hint"))
        .checked(on);
    if m.add(item).clicked() {
        picked = Some(pick(chosen, &system));
    }
    m.separator();
    match &*audio::lock(apps) {
        Listing::Loading => {
            m.ui().horizontal(|ui| {
                ui.add_space(10.0);
                ui.spinner();
                ui.weak(t!("call.looking_for_apps"));
            });
        }
        Listing::Failed => empty(m.ui(), t!("call.apps_failed")),
        Listing::Found(apps) if apps.is_empty() => {
            empty(m.ui(), t!("call.apps_empty"));
        }
        Listing::Found(apps) => {
            let c = ui::colors(m.ui());
            // A soft pulse on each dot: these apps are playing right now.
            let time = m.ui().input(|input| input.time);
            let pulse = 0.65 + 0.35 * (time * 3.0).sin().abs() as f32;
            m.ui().ctx().request_repaint_after_secs(0.05);
            for app in apps {
                let was = chosen.iter().any(|shared| shared.id == app.id);
                let item = ui::Item::dot(c.success.gamma_multiply(pulse), &app.name)
                    .hint(t!("call.playing"))
                    .checked(was);
                if m.add(item).clicked() {
                    picked = Some(pick(chosen, app));
                }
            }
        }
    }
    m.separator();
    let refresh = m.item("🔄", t!("call.refresh")).clicked();
    (picked, refresh)
}

/// Toggle `app` in `chosen`. All system audio already holds every app, so it is
/// either the only pick or not picked at all.
fn pick(chosen: &[App], app: &App) -> Vec<App> {
    let on = chosen.iter().any(|c| c.id == app.id);
    if share::is_system(app) {
        return if on { Vec::new() } else { vec![app.clone()] };
    }
    let mut next: Vec<_> = (chosen.iter())
        .filter(|c| !share::is_system(c) && c.id != app.id)
        .cloned()
        .collect();
    if !on {
        next.push(app.clone());
    }
    next
}

/// What to turn off and on to get from `before` to `after`.
fn diff(before: &[App], after: &[App]) -> Vec<(App, bool)> {
    let missing = |list: &[App], app: &App| !list.iter().any(|a| a.id == app.id);
    (before.iter().filter(|a| missing(after, a)))
        .map(|a| (a.clone(), false))
        .chain(
            after
                .iter()
                .filter(|a| missing(before, a))
                .map(|a| (a.clone(), true)),
        )
        .collect()
}

/// The stream's sound in a few words, for its picker button.
pub(super) fn sound_summary(chosen: &[App]) -> String {
    if chosen.is_empty() {
        return t!("call.no_sound").into();
    }
    let names: Vec<_> = chosen.iter().map(|app| app.name.as_str()).collect();
    names.join(", ")
}

/// A line of help in the app picker when there is nothing to pick.
fn empty(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.add(Label::new(RichText::new(text).color(ui::colors(ui).muted)).wrap());
    });
}

/// t!("call.now_sharing") above the panel while this device shares its screen or app audio,
/// with controls for each active share.
/// `blocked` says why the stream's sound can't be picked now, if it can't.
fn sharing_card(ui: &mut Ui, voice: &Voice, blocked: Option<&str>) -> Option<Stop> {
    let shown = voice.sharing != Sharing::Off || !voice.shares.is_empty();
    let mut stop = None;
    ui::fade_card(ui, "now sharing", shown, |ui| {
        ui.label(ui::caption(t!("call.now_sharing")));
        let screen = match voice.sharing {
            Sharing::Off => None,
            Sharing::Starting => Some(t!("call.starting_screen").to_owned()),
            Sharing::Changing | Sharing::On => Some(t!("call.your_screen").to_owned()),
        };
        if let Some(text) = screen {
            ui::right_row(ui, STOP_SIZE.y, |ui| {
                if voice.sharing != Sharing::Starting {
                    stop = screen_tools(ui, voice, blocked);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui::dot(ui, ui::colors(ui).success);
                    ui.add(Label::new(format!("🖥  {text}")).truncate());
                });
            });
        }
        for app in &voice.shares {
            ui::right_row(ui, STOP_SIZE.y, |ui| {
                let tip = t!("call.stop_sharing_app", app = app.name);
                if ui::icon_button(ui, "⏹", &tip, Look::Plain, STOP_SIZE).clicked() {
                    stop = Some(Stop::App(app.id.clone()));
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui::dot(ui, ui::colors(ui).success);
                    ui.add(Label::new(format!("🎵  {}", app.name)).truncate());
                });
            });
        }
    });
    ui.add_space(4.0);
    stop
}

/// The screen share's sound and source controls, laid out right to left.
fn screen_tools(ui: &mut Ui, voice: &Voice, blocked: Option<&str>) -> Option<Stop> {
    let mut stop = None;
    let (glyph, look) = if voice.stream_audio.is_empty() {
        ("🔇", Look::Plain)
    } else {
        ("🔊", Look::On(ui::colors(ui).success))
    };
    let tip = format!(
        "{}: {}",
        t!("call.stream_sound"),
        sound_summary(&voice.stream_audio)
    );
    let sound = ui
        .add_enabled_ui(voice.supported && blocked.is_none(), |ui| {
            ui::icon_button(ui, glyph, &tip, look, STOP_SIZE)
        })
        .inner;
    let sound = match blocked {
        Some(why) => sound.on_disabled_hover_text(why),
        None => sound,
    };
    if sound.clicked() {
        fill(ui.ctx(), &voice.apps, "apps", share::list);
    }
    let (picked, refresh) = sound_menu(&sound, &voice.apps, &voice.stream_audio);
    if let Some(picked) = picked {
        stop = Some(Stop::Sound(picked));
    }
    if refresh {
        fill(ui.ctx(), &voice.apps, "apps", share::list);
    }
    let picking = voice.sharing == Sharing::Changing;
    let change = ui.add_enabled_ui(!picking, |ui| {
        ui::icon_button(ui, "🔄", t!("call.change_share"), Look::Plain, STOP_SIZE)
    });
    if change.inner.clicked() {
        stop = Some(Stop::Change);
    }
    stop
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(ping: Option<f64>, lost: Option<i64>, received: Option<u64>) -> Stats {
        Stats {
            ping_ms: ping,
            lost_in: lost,
            received_in: received,
            ..Stats::default()
        }
    }

    #[test]
    fn quality_from_ping_and_loss() {
        assert_eq!(quality(None, Some(0.0)), Quality::Unknown);
        assert_eq!(quality(Some(40.0), None), Quality::Good);
        assert_eq!(quality(Some(40.0), Some(1.9)), Quality::Good);
        assert_eq!(quality(Some(40.0), Some(2.0)), Quality::Fair);
        assert_eq!(quality(Some(200.0), Some(0.0)), Quality::Fair);
        assert_eq!(quality(Some(350.0), Some(0.0)), Quality::Poor);
        assert_eq!(quality(Some(40.0), Some(10.0)), Quality::Poor);
    }

    #[test]
    fn ping_history_keeps_five_minutes() {
        let mut link = Link::default();
        assert_eq!(link.average(), None);
        for ping in 0..HISTORY + 10 {
            link.push(&stats(Some(ping as f64), None, None));
        }
        link.push(&stats(None, None, None));
        assert_eq!(link.pings.len(), HISTORY);
        assert_eq!(link.pings.front(), Some(&10.0));
        assert_eq!(link.pings.back(), Some(&(HISTORY as f64 + 9.0)));
        assert_eq!(link.average(), Some(10.0 + (HISTORY as f64 - 1.0) / 2.0));
    }

    #[test]
    fn loss_is_measured_between_samples() {
        let mut link = Link::default();
        link.push(&stats(None, Some(10), Some(1_000)));
        assert_eq!(link.loss, None);
        link.push(&stats(None, Some(15), Some(1_095)));
        assert_eq!(link.loss, Some(5.0));
        // Someone left: counters dropped, keep the last value and start again.
        link.push(&stats(None, Some(2), Some(300)));
        assert_eq!(link.loss, Some(5.0));
        link.push(&stats(None, Some(2), Some(400)));
        assert_eq!(link.loss, Some(0.0));
        link.push(&stats(None, None, None));
        assert_eq!(link.loss, None);
    }

    fn app(id: &str) -> App {
        App {
            id: id.into(),
            name: id.into(),
        }
    }

    #[test]
    fn system_audio_and_apps_exclude_each_other() {
        // Picks by ID: "system" is all system audio.
        let apps = |ids: &[&str]| -> Vec<App> {
            (ids.iter())
                .map(|id| {
                    if *id == "system" {
                        share::system()
                    } else {
                        app(id)
                    }
                })
                .collect()
        };
        let ids = |apps: Vec<App>| -> Vec<String> { apps.into_iter().map(|a| a.id).collect() };
        let toggle = |chosen: &[&str], id: &str| ids(pick(&apps(chosen), &apps(&[id])[0]));
        assert_eq!(toggle(&[], "a"), ["a"]);
        assert_eq!(toggle(&["a"], "b"), ["a", "b"]);
        assert_eq!(toggle(&["a", "b"], "a"), ["b"]);
        // All system audio replaces the apps, and an app replaces it.
        assert_eq!(toggle(&["a", "b"], "system"), ["system"]);
        assert_eq!(toggle(&["system"], "a"), ["a"]);
        assert!(toggle(&["system"], "system").is_empty());
        // The app picker turns those into stops and starts.
        let changes: Vec<_> = diff(&apps(&["a", "b"]), &apps(&["system"]))
            .into_iter()
            .map(|(app, on)| (app.id, on))
            .collect();
        let expected = [("a", false), ("b", false), ("system", true)];
        assert_eq!(changes, expected.map(|(id, on)| (id.to_owned(), on)));
        assert_eq!(sound_summary(&[]), "No sound");
        assert_eq!(sound_summary(&apps(&["Spotify", "Game"])), "Spotify, Game");
    }

    #[test]
    fn tooltips_follow_the_state() {
        assert_eq!(camera_tip(false), "Turn on camera");
        assert_eq!(camera_tip(true), "Turn off camera");
        assert_eq!(screen_tip(Sharing::Off), "Share your screen");
        assert_eq!(screen_tip(Sharing::Starting), "Stop sharing");
        assert_eq!(screen_tip(Sharing::Changing), "Stop sharing");
        assert_eq!(screen_tip(Sharing::On), "Stop sharing");
    }
}

#[cfg(test)]
mod phone_consent_tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn external_phone_notice_requires_a_separate_action_in_both_themes() {
        for (name, dark) in [("dark", true), ("light", false)] {
            let builder = Harness::builder()
                .with_theme(if dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                })
                .with_size(egui::vec2(380.0, 220.0))
                .with_context_setup(move |ctx| {
                    ui::install(ctx);
                    ui::apply(
                        ctx,
                        ui::Appearance {
                            mode: if dark {
                                ui::Mode::Dark
                            } else {
                                ui::Mode::Light
                            },
                            ..Default::default()
                        },
                    );
                });
            let builder = if std::env::var_os("RUSTTALK_PHONE_SCREENSHOTS").is_some() {
                builder.wgpu()
            } else {
                builder
            };
            use crate::phone::ConsentStatus;
            let mut harness = builder.build_ui_state(
                |ui, status| match external_phone_notice(ui, "Phone", *status) {
                    Some(PhonePick::Toggle) if *status == ConsentStatus::Off => {
                        *status = ConsentStatus::On;
                    }
                    Some(PhonePick::Toggle) => *status = ConsentStatus::Off,
                    Some(PhonePick::Always) => *status = ConsentStatus::On,
                    None => {}
                },
                ConsentStatus::Off,
            );
            harness.run_steps(3);
            assert_eq!(*harness.state(), ConsentStatus::Off);
            harness.get_by_label(t!("phone.external_help"));
            if let Ok(dir) = std::env::var("RUSTTALK_PHONE_SCREENSHOTS") {
                harness
                    .render()
                    .unwrap()
                    .save(std::path::Path::new(&dir).join(format!("phone-consent-{name}.png")))
                    .unwrap();
            }
            harness.get_by_label(t!("phone.allow")).click();
            harness.run_steps(2);
            assert_eq!(*harness.state(), ConsentStatus::On);
            // Once shared, there is nothing left to always allow.
            assert!(harness.query_by_label(t!("phone.always_allow")).is_none());
            harness.get_by_label(t!("phone.withdraw")).click();
            harness.run_steps(2);
            assert_eq!(*harness.state(), ConsentStatus::Off);
            harness.get_by_label(t!("phone.always_allow")).click();
            harness.run_steps(2);
            assert_eq!(*harness.state(), ConsentStatus::On);
            for (status, key, suffix) in [
                (
                    ConsentStatus::Withdrawing,
                    t!("phone.withdrawing"),
                    "pending",
                ),
                (ConsentStatus::Failed, t!("phone.withdraw_failed"), "failed"),
            ] {
                *harness.state_mut() = status;
                harness.run_steps(3);
                harness.get_by_label(key);
                assert!(harness.query_by_label(t!("phone.allow")).is_none());
                assert!(harness.query_by_label(t!("phone.withdraw")).is_none());
                if let Ok(dir) = std::env::var("RUSTTALK_PHONE_SCREENSHOTS") {
                    harness
                        .render()
                        .unwrap()
                        .save(
                            std::path::Path::new(&dir)
                                .join(format!("phone-consent-{suffix}-{name}.png")),
                        )
                        .unwrap();
                }
            }
        }
    }
}
