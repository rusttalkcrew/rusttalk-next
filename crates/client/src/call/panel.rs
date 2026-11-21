//! The voice panel at the bottom of the channel list: a header with connection
//! quality and hang-up, one row of share buttons, the "now sharing" card above it
//! and the connection popup.

use std::collections::VecDeque;

use eframe::egui::{
    self, Align, Color32, CursorIcon, Frame, Grid, Label, Layout, Margin, Response, RichText, Role,
    Sense, Ui, Vec2, WidgetInfo,
};
use rusttalk_core::client::voice::{CallTraffic, TrackSource, Traffic};

use super::{
    Listing, LiveAction, Sharing, Voice, audio, change_screen as change_screen_now, fill, leave,
    live_files, open_screen_dialog, screen, status_name,
};
use crate::{
    files::Ask,
    home::{Home, Tab},
    net::Command,
    server::{CallState, Id, Server, Stats},
    share::{self, App},
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
            Quality::Unknown => "Measuring…",
            Quality::Poor => "Poor",
            Quality::Fair => "Okay",
            Quality::Good => "Good",
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
        "Turn off camera"
    } else {
        "Turn on camera"
    }
}

fn screen_tip(sharing: Sharing) -> &'static str {
    match sharing {
        Sharing::Off => "Share your screen",
        Sharing::Starting | Sharing::Changing | Sharing::On => "Stop sharing",
    }
}

/// What a button on the "now sharing" card does.
enum Stop {
    Screen,
    /// Pick another screen to share.
    Change,
    App(String),
}

/// The voice panel, while in a voice room.
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
    let c = ui::colors(ui);
    let (text, color) = match call.state {
        CallState::Connecting => ("Connecting…", c.warning),
        CallState::Connected => ("Voice connected", c.success),
        CallState::Reconnecting => ("Reconnecting…", c.warning),
    };
    let connected = call.state == CallState::Connected;
    let can_talk = call.can_talk;
    let can_share_video = call.can_share_video;
    let stats = call.stats.clone();
    let channel = call.channel;
    let quality = if connected {
        quality(stats.ping_ms, home.voice.link.loss)
    } else {
        Quality::Unknown
    };
    let stopped = sharing_card(ui, &home.voice);
    let mut live = None;
    let mut leave_now = false;
    let mut share = Vec::new();
    let mut stop_screen = matches!(stopped, Some(Stop::Screen));
    let mut change_screen = matches!(stopped, Some(Stop::Change));
    let mut voice_settings = false;
    Frame::new()
        .fill(c.field)
        .corner_radius(RADIUS)
        .inner_margin(Margin::same(ui::SPACE_S as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui::right_row(ui, ICON_BUTTON, |ui| {
                let hang = ui::icon_button(ui, "📞", "Leave call", Look::Danger, SIGNAL_SIZE);
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
            let count = if screen::SUPPORTED { 3.0 } else { 2.0 };
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
                    button.on_disabled_hover_text("You can't share video here");
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
                            if m.item("🖥", "Change what I share").clicked() {
                                change_screen = true;
                            }
                            if m.danger("⏹", "Stop sharing").clicked() {
                                stop_screen = true;
                            }
                        });
                    }
                    if !can_share_video {
                        button.on_disabled_hover_text("You can't share your screen here");
                    } else if button.clicked() {
                        if voice.sharing == Sharing::Off {
                            open_screen_dialog(ui.ctx(), voice);
                        } else {
                            stop_screen = true;
                        }
                    }
                }
                share = app_button(ui, voice, connected && can_talk, can_talk, size);
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
    if voice_settings {
        home.settings = Some(Tab::Voice);
    }
    if leave_now {
        leave(home);
    }
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
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, "Connection info"));
    response
        .on_hover_text(format!("Connection info · {}", quality.label()))
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
        ui.label(ui::bold("Connection"));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(quality.label()).color(color));
            ui::dot(ui, color);
        });
    });
    Grid::new("connection").num_columns(2).show(ui, |ui| {
        ui.label("Ping");
        ui.label(ms(stats.ping_ms));
        ui.end_row();
        ui.label("Average");
        ui.label(ms(link.average()));
        ui.end_row();
        ui.label("Packet loss");
        ui.label(link.loss.map_or("—".into(), |v| format!("{v:.1} %")));
        ui.end_row();
    });
    ui.add_space(4.0);
    let pings: Vec<f64> = link.pings.iter().copied().collect();
    let size = Vec2::new(ui.available_width(), CHART_HEIGHT);
    ui::line_chart(ui, &pings, HISTORY, size, color);
    ui.horizontal(|ui| {
        ui.label(ui::caption("5 min ago"));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(ui::caption("now"));
        });
    });
    ui.separator();
    if let Some(server) = server
        && let Some(peers) = server.voice.get(channel)
        && peers.iter().any(|peer| peer.person != server.dir.me)
    {
        egui::CollapsingHeader::new("People in call").show(ui, |ui| {
            for peer in peers.iter().filter(|peer| peer.person != server.dir.me) {
                ui.horizontal(|ui| status_name(ui, &peer.name(server), peer.status));
            }
        });
    }
    traffic(ui, &stats.traffic, link);
    egui::CollapsingHeader::new("Details")
        .id_salt("connection details")
        .show(ui, |ui| details(ui, stats, server));
    ui.separator();
    ui.label("🔒  End-to-end encrypted")
        .on_hover_text("Only people in the call can hear and see it, not the server.");
    ui::secondary(ui, "⚙  Voice settings").clicked()
}

fn traffic(ui: &mut Ui, traffic: &CallTraffic, link: &Link) {
    let c = ui::colors(ui);
    Grid::new("call traffic")
        .num_columns(3)
        .min_row_height(ui::TEXT_BODY)
        .spacing(Vec2::new(ui::SPACE_S, SPACE_XS))
        .show(ui, |ui| {
            ui.label(ui::bold("Traffic").size(ui::TEXT_CAPTION));
            ui.label(ui::caption("Upload"));
            ui.label(ui::caption("Download"));
            ui.end_row();
            let mut row = |label: &str, pair: [Traffic; 2]| {
                ui.label(ui::caption(label).color(c.text));
                for value in pair {
                    let rate = if value.bps >= 1_000_000.0 {
                        format!("{:.1} Mbps", value.bps / 1_000_000.0)
                    } else {
                        format!("{:.0} kbps", value.bps / 1000.0)
                    };
                    let mb = value.bytes as f64 / 1_000_000.0;
                    ui.label(ui::caption(format!("{rate} · {mb:.2} MB")).color(c.text));
                }
                ui.end_row();
            };
            row("Total", traffic.total());
            for (label, source) in [
                ("Voice", TrackSource::Microphone),
                ("Camera", TrackSource::Camera),
                ("Screen", TrackSource::Screenshare),
                ("App audio", TrackSource::ScreenshareAudio),
            ] {
                row(label, traffic.for_source(source));
            }
        });
    ui.label(ui::caption("Upload + download · last 5 min"));
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
            ui.label("Jitter");
            ui.label(ms(stats.jitter_ms));
            ui.end_row();
            ui.label("Lost packets in");
            ui.label(count(stats.lost_in));
            ui.end_row();
            ui.label("Lost packets out");
            ui.label(count(stats.lost_out));
            ui.end_row();
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
}

/// The app audio button and its picker: each app playing sound with a check, a
/// refresh and "Stop sharing all". Returns the apps turned on or off.
fn app_button(
    ui: &mut Ui,
    voice: &mut Voice,
    enabled: bool,
    can_talk: bool,
    size: Vec2,
) -> Vec<(App, bool)> {
    let look = if voice.shares.is_empty() {
        Look::Plain
    } else {
        Look::On(ui::colors(ui).success)
    };
    let usable = voice.supported && enabled;
    let button = ui
        .add_enabled_ui(usable, |ui| {
            ui::icon_button(ui, "🎵", "Share app audio", look, size)
        })
        .inner;
    let button = if !voice.supported {
        button.on_disabled_hover_text("Needs Windows 11")
    } else if !can_talk {
        button.on_disabled_hover_text("You can't talk here")
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
        m.caption("Share app audio");
        match &*audio::lock(&voice.apps) {
            Listing::Loading => {
                m.ui().horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.spinner();
                    ui.weak("Looking for apps…");
                });
            }
            Listing::Failed => empty(m.ui(), "Couldn't look for apps."),
            Listing::Found(apps) if apps.is_empty() => {
                empty(
                    m.ui(),
                    "Start playing something in an app, then pick it here.",
                );
            }
            Listing::Found(apps) => {
                let c = ui::colors(m.ui());
                // A soft pulse on each dot: these apps are playing right now.
                let time = m.ui().input(|input| input.time);
                let pulse = 0.65 + 0.35 * (time * 3.0).sin().abs() as f32;
                m.ui().ctx().request_repaint_after_secs(0.05);
                for app in apps {
                    let was = voice.shares.iter().any(|shared| shared.id == app.id);
                    let item = ui::Item::dot(c.success.gamma_multiply(pulse), &app.name)
                        .hint("Playing")
                        .checked(was);
                    if m.add(item).clicked() {
                        changes.push((app.clone(), !was));
                    }
                }
            }
        }
        m.separator();
        refresh = m.item("🔄", "Refresh").clicked();
        if !voice.shares.is_empty() && m.danger("⏹", "Stop sharing all").clicked() {
            changes.extend(voice.shares.iter().map(|app| (app.clone(), false)));
        }
    });
    if refresh {
        list(ui);
    }
    changes
}

/// A line of help in the app picker when there is nothing to pick.
fn empty(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.add(Label::new(RichText::new(text).color(ui::colors(ui).muted)).wrap());
    });
}

/// "Now sharing" above the panel while this device shares its screen or app audio,
/// each with a stop button.
fn sharing_card(ui: &mut Ui, voice: &Voice) -> Option<Stop> {
    let shown = voice.sharing != Sharing::Off || !voice.shares.is_empty();
    let mut stop = None;
    ui::fade_card(ui, "now sharing", shown, |ui| {
        ui.label(ui::caption("Now sharing"));
        let screen = match voice.sharing {
            Sharing::Off => None,
            Sharing::Starting => Some("Starting your screen…".to_owned()),
            Sharing::Changing | Sharing::On => Some("Your screen".to_owned()),
        };
        let rows = screen
            .map(|text| {
                (
                    "🖥",
                    text,
                    "Stop sharing your screen".to_owned(),
                    Stop::Screen,
                )
            })
            .into_iter()
            .chain(voice.shares.iter().map(|app| {
                let tip = format!("Stop sharing {}", app.name);
                ("🎵", app.name.clone(), tip, Stop::App(app.id.clone()))
            }));
        for (icon, text, tip, which) in rows {
            ui::right_row(ui, STOP_SIZE.y, |ui| {
                let screen_row = matches!(which, Stop::Screen);
                if ui::icon_button(ui, "⏹", &tip, Look::Plain, STOP_SIZE).clicked() {
                    stop = Some(which);
                }
                if screen_row && voice.sharing != Sharing::Starting {
                    let picking = voice.sharing == Sharing::Changing;
                    let change = ui.add_enabled_ui(!picking, |ui| {
                        ui::icon_button(ui, "🔄", "Change what you share", Look::Plain, STOP_SIZE)
                    });
                    if change.inner.clicked() {
                        stop = Some(Stop::Change);
                    }
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui::dot(ui, ui::colors(ui).success);
                    ui.add(Label::new(format!("{icon}  {text}")).truncate());
                });
            });
        }
    });
    ui.add_space(4.0);
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
