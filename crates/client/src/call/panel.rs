//! The voice panel at the bottom of the channel list: a header with connection
//! quality and hang-up, one row of share buttons, the "now sharing" card above it
//! and the connection popup.

use std::{collections::VecDeque, sync::Mutex};

use eframe::egui::{
    self, Align, Color32, CursorIcon, Frame, Grid, Label, Layout, Margin, Response, RichText, Role,
    Sense, Ui, Vec2, WidgetInfo,
};
use rusttalk_core::client::voice::{CallTraffic, TrackSource, Traffic};

use super::{
    Listing, LiveAction, Row, Sharing, Voice, audio, change_screen as change_screen_now, fill,
    leave, live_files, open_screen_dialog, screen, status_name,
};
use crate::{
    files::Ask,
    home::{Home, Tab},
    mic_check::Verdict,
    net::Command,
    server::{CallState, Id, Server, Stats},
    share::{self, App},
    terminal::Shell,
    ui::{self, HOVER_SECONDS, ICON_BUTTON, Look, Palette, RADIUS, SPACE_XS},
};

/// Samples kept for the charts: five minutes at one every two seconds.
pub const HISTORY: usize = 150;
/// Up to these, the connection counts as good, then as fair.
const GOOD_PING_MS: f64 = 150.0;
const FAIR_PING_MS: f64 = 300.0;
const GOOD_LOSS_PERCENT: f64 = 2.0;
const FAIR_LOSS_PERCENT: f64 = 8.0;
const SIGNAL_SIZE: Vec2 = Vec2::splat(ICON_BUTTON);
const CHART_HEIGHT: f32 = 56.0;
const TRAFFIC_CHART_HEIGHT: f32 = 40.0;
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
    Screen,
    /// Pick another screen to share.
    Change,
    App(String),
    /// The screen share's sound changed to these.
    Sound(Vec<App>),
    /// Show or hide the preview of our own screen.
    Preview,
}

/// The voice panel, while in a voice room.
pub fn call_bar(ui: &mut Ui, home: &mut Home) {
    if home.voice.call.is_none() {
        return;
    }
    mic_hint_card(ui, home);
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
    let connected = call.state == CallState::Connected;
    let can_talk = call.can_talk;
    // A moderator's mute also takes shared app audio.
    let blocked = if server.is_some_and(|server| server.voice_muted(&server.dir.me)) {
        Some(t!("call.server_muted"))
    } else if !can_talk {
        Some(t!("call.no_talk_here"))
    } else {
        None
    };
    let can_share_video = call.can_share_video;
    let stats = call.stats.clone();
    let channel = call.channel;
    let quality = if connected {
        quality(stats.ping_ms, home.voice.link.loss)
    } else {
        Quality::Unknown
    };
    let stopped = sharing_card(ui, &home.voice, blocked);
    if let Some(Stop::Sound(apps)) = &stopped
        && let Some(server) = (home.voice.call.as_ref())
            .and_then(|call| home.servers.iter().find(|s| s.address == call.server))
    {
        home.voice.stream_audio = apps.clone();
        server.send(Command::StreamAudio(apps.clone()));
    }
    let mut live = None;
    let mut leave_now = false;
    let mut share = Vec::new();
    let mut stop_screen = matches!(stopped, Some(Stop::Screen));
    let mut change_screen = matches!(stopped, Some(Stop::Change));
    if matches!(stopped, Some(Stop::Preview)) {
        let preview = &home.voice.video.preview;
        if preview.is_on() {
            home.voice.stop_preview();
        } else {
            preview.set(true);
        }
    }
    let mut voice_settings = false;
    let mut terminal = None;
    Frame::new()
        .fill(c.field)
        .corner_radius(RADIUS)
        .inner_margin(Margin::same(ui::SPACE_S as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui::right_row(ui, ICON_BUTTON, |ui| {
                let hang = ui::icon_button(ui, "📞", t!("call.leave"), Look::Danger, SIGNAL_SIZE);
                leave_now = hang.clicked();
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let signal = signal_button(ui, quality);
                    ui::popup(&signal, |ui| {
                        voice_settings =
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
            let more = home.voice.audio.can_host_terminal();
            let count = 2.0 + f32::from(screen::SUPPORTED) + f32::from(more);
            let width = (ui.available_width() - SPACE_XS * (count - 1.0)) / count;
            let size = Vec2::new(width, ICON_BUTTON);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = SPACE_XS;
                let voice = &mut home.voice;
                let camera = &mut voice.camera;
                let look = if camera.on {
                    Look::On(c.success)
                } else {
                    Look::Plain
                };
                let button = ui
                    .add_enabled_ui(connected && can_share_video, |ui| {
                        ui::icon_button(ui, "📷", camera_tip(camera.on), look, size)
                    })
                    .inner;
                if !can_share_video {
                    button.on_disabled_hover_text(t!("call.no_video_here"));
                } else if button.clicked() {
                    camera.on ^= true;
                }
                if screen::SUPPORTED {
                    let look = if voice.sharing == Sharing::Off {
                        Look::Plain
                    } else {
                        Look::On(c.success)
                    };
                    let button = ui
                        .add_enabled_ui(connected && can_share_video, |ui| {
                            ui::icon_button(ui, "🖥", screen_tip(voice.sharing), look, size)
                        })
                        .inner;
                    if voice.sharing == Sharing::On {
                        ui::context_menu(&button, |m| {
                            if m.item("🖥", t!("call.change_my_share")).clicked() {
                                change_screen = true;
                            }
                            if m.danger("⏹", t!("call.stop_sharing")).clicked() {
                                stop_screen = true;
                            }
                        });
                    }
                    if !can_share_video {
                        button.on_disabled_hover_text(t!("call.no_screen_here"));
                    } else if button.clicked() {
                        if voice.sharing == Sharing::Off {
                            open_screen_dialog(ui.ctx(), voice);
                        } else {
                            stop_screen = true;
                        }
                    }
                }
                share = app_button(ui, voice, connected && blocked.is_none(), blocked, size);
                if more {
                    terminal = more_button(ui, voice.terminals.local.is_some(), connected, size);
                }
            });
            if let Some(action) = live_files(ui, &home.voice.live_files, server) {
                live = Some(action);
            }
        });
    let server = home
        .voice
        .call
        .as_ref()
        .and_then(|call| home.servers.iter().find(|s| s.address == call.server));
    for (app, on) in share {
        let Some(server) = server else { break };
        if on {
            home.voice.shares.push(app.clone());
            server.send(Command::ShareApp(app));
        } else {
            home.voice.shares.retain(|shared| shared.id != app.id);
            server.send(Command::StopApp(app.id.clone()));
        }
    }
    if let (Some(server), Some(Stop::App(id))) = (server, stopped) {
        home.voice.shares.retain(|shared| shared.id != id);
        server.send(Command::StopApp(id));
    }
    if let (Some(server), true) = (server, change_screen) {
        change_screen_now(ui.ctx(), &mut home.voice, server);
    }
    if let (Some(server), true) = (server, stop_screen) {
        home.voice.sharing = Sharing::Off;
        home.voice.stop_preview();
        server.send(Command::StopScreen);
    }
    if let Some(server) = server {
        let address = server.address.clone();
        match live {
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
    if let Some(shell) = terminal {
        super::terminal::toggle(ui.ctx(), home, shell);
    }
    if voice_settings {
        home.settings = Some(Tab::Voice);
    }
    if leave_now {
        leave(home);
    }
}

/// The "More" button: a menu for the less common shares, so the row stays short. Starts or
/// stops a terminal share; on Windows it also offers Command Prompt.
fn more_button(ui: &mut Ui, on: bool, connected: bool, size: Vec2) -> Option<Shell> {
    let button = ui
        .add_enabled_ui(connected, |ui| {
            ui::icon_button(ui, "…", t!("call.more"), Look::Plain, size)
        })
        .inner;
    let mut picked = None;
    ui::menu_wide(&button, |m| {
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
    picked
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
    let ms = |value: Option<f64>| value.map_or("—".into(), |v| format!("{v:.0} ms"));
    let color = quality.color(ui::colors(ui));
    ui.horizontal(|ui| {
        ui.label(ui::bold(t!("call.connection")));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(quality.label()).color(color));
            ui::dot(ui, color);
        });
    });
    Grid::new("connection").num_columns(2).show(ui, |ui| {
        ui.label(t!("call.ping"));
        ui.label(ms(stats.ping_ms));
        ui.end_row();
        ui.label(t!("call.average"));
        ui.label(ms(link.average()));
        ui.end_row();
        ui.label(t!("call.packet_loss"));
        ui.label(
            link.loss
                .map_or("—".into(), |v| format!("{} %", crate::i18n::number(v, 1))),
        );
        ui.end_row();
    });
    ui.add_space(4.0);
    let pings: Vec<f64> = link.pings.iter().copied().collect();
    let size = Vec2::new(ui.available_width(), CHART_HEIGHT);
    ui::line_chart(ui, &pings, HISTORY, size, color);
    ui.horizontal(|ui| {
        ui.label(ui::caption(t!("call.five_min_ago")));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(ui::caption(t!("call.now")));
        });
    });
    ui.separator();
    if let Some(server) = server
        && let Some(peers) = server.voice.get(channel)
        && peers.iter().any(|peer| peer.person != server.dir.me)
    {
        egui::CollapsingHeader::new(t!("call.people_in_call")).show(ui, |ui| {
            for peer in peers.iter().filter(|peer| peer.person != server.dir.me) {
                let muted = server.voice_muted(&peer.person);
                ui.horizontal(|ui| status_name(ui, &peer.name(server), peer.status, muted));
            }
        });
    }
    traffic(ui, &stats.traffic, link);
    egui::CollapsingHeader::new(t!("common.details"))
        .id_salt("connection details")
        .show(ui, |ui| details(ui, stats, server));
    ui.separator();
    ui.label(format!("🔒  {}", t!("call.e2ee")))
        .on_hover_text(t!("call.e2ee_help"));
    ui::secondary(ui, &format!("⚙  {}", t!("call.voice_settings"))).clicked()
}

fn traffic(ui: &mut Ui, traffic: &CallTraffic, link: &Link) {
    let c = ui::colors(ui);
    Grid::new("call traffic")
        .num_columns(3)
        .min_row_height(ui::TEXT_BODY)
        .spacing(Vec2::new(ui::SPACE_S, SPACE_XS))
        .show(ui, |ui| {
            ui.label(ui::bold(t!("call.traffic")).size(ui::TEXT_CAPTION));
            ui.label(ui::caption(t!("call.upload")));
            ui.label(ui::caption(t!("call.download_traffic")));
            ui.end_row();
            let mut row = |label: &str, pair: [Traffic; 2]| {
                ui.label(ui::caption(label).color(c.text));
                for value in pair {
                    let rate = if value.bps >= 1_000_000.0 {
                        format!("{} Mbps", crate::i18n::number(value.bps / 1_000_000.0, 1))
                    } else {
                        format!("{:.0} kbps", value.bps / 1000.0)
                    };
                    let mb = value.bytes as f64 / 1_000_000.0;
                    ui.label(
                        ui::caption(format!("{rate} · {} MB", crate::i18n::number(mb, 2)))
                            .color(c.text),
                    );
                }
                ui.end_row();
            };
            row(t!("call.total"), traffic.total());
            for (label, source) in [
                (t!("call.track_voice"), TrackSource::Microphone),
                (t!("call.track_camera"), TrackSource::Camera),
                (t!("call.track_screen"), TrackSource::Screenshare),
                (t!("call.track_app_audio"), TrackSource::ScreenshareAudio),
            ] {
                row(label, traffic.for_source(source));
            }
        });
    ui.label(ui::caption(t!("call.traffic_caption")));
    let rates: Vec<_> = link.traffic_kbps.iter().copied().collect();
    let size = Vec2::new(ui.available_width(), TRAFFIC_CHART_HEIGHT);
    ui::line_chart(ui, &rates, HISTORY, size, ui::accent(ui).text(c.dark));
}

/// Jitter, lost packets and bitrates: what the connection window used to show.
fn details(ui: &mut Ui, stats: &Stats, server: Option<&Server>) {
    let ms = |value: Option<f64>| value.map_or("—".into(), |v| format!("{v:.0} ms"));
    let count = |value: Option<i64>| value.map_or("—".into(), |v| v.to_string());
    Grid::new("connection details")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label(t!("call.jitter"));
            ui.label(ms(stats.jitter_ms));
            ui.end_row();
            ui.label(t!("call.lost_in"));
            ui.label(count(stats.lost_in));
            ui.end_row();
            ui.label(t!("call.lost_out"));
            ui.label(count(stats.lost_out));
            ui.end_row();
            for track in &stats.tracks {
                let name = match (track.mine, server) {
                    (true, _) => t!("common.you").to_owned(),
                    (false, Some(server)) => server.member_name(&track.person),
                    (false, None) => String::new(),
                };
                match &track.app {
                    Some(app) => ui.label(format!("{name} · {app}")),
                    None => ui.label(name),
                };
                let rate = format!("{:.0} kbps", track.kbps);
                ui.label(match track.video {
                    // What a video really runs at: a share set to 60 fps may arrive slower.
                    Some(v) => format!("{}×{} · {:.0} fps · {rate}", v.width, v.height, v.fps),
                    None => rate,
                });
                ui.end_row();
            }
        });
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
/// each with a stop button.
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
        let rows = screen
            .map(|text| {
                (
                    "🖥",
                    text,
                    t!("call.stop_sharing_screen").to_owned(),
                    Stop::Screen,
                )
            })
            .into_iter()
            .chain(voice.shares.iter().map(|app| {
                let tip = t!("call.stop_sharing_app", app = app.name);
                ("🎵", app.name.clone(), tip, Stop::App(app.id.clone()))
            }));
        for (icon, text, tip, which) in rows {
            // The screen's other buttons get a row of their own, so its name stays
            // readable in a narrow panel.
            let tools = matches!(which, Stop::Screen) && voice.sharing != Sharing::Starting;
            ui::right_row(ui, STOP_SIZE.y, |ui| {
                if ui::icon_button(ui, "⏹", &tip, Look::Plain, STOP_SIZE).clicked() {
                    stop = Some(which);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui::dot(ui, ui::colors(ui).success);
                    ui.add(Label::new(format!("{icon}  {text}")).truncate());
                });
            });
            if tools {
                ui::right_row(ui, STOP_SIZE.y, |ui| {
                    stop = screen_tools(ui, voice, blocked).or(stop.take());
                });
            }
        }
    });
    ui.add_space(4.0);
    stop
}

/// The screen share's buttons on the "now sharing" card: its sound, the preview and
/// changing what is shared, laid out right to left. `blocked` says why the sound can't be
/// picked now, if it can't.
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
    let look = if voice.video.preview.is_on() {
        Look::On(ui::accent(ui).base)
    } else {
        Look::Plain
    };
    if ui::icon_button(ui, "👁", t!("call.preview"), look, STOP_SIZE).clicked() {
        stop = Some(Stop::Preview);
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
