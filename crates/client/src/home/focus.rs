//! Focus mode (#387): a channel asks the people joining it not to interrupt, until a
//! set time. The end time is plain channel state on the server; nothing written is sent.

use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveTime, TimeZone};
use eframe::egui::{
    self, Align, Color32, FontId, Frame, Layout, Margin, Rect, RichText, Ui, UiBuilder, Vec2,
};

use super::{Home, events::parse_time};
use crate::{
    call,
    net::Command,
    notify::unix_now,
    server::{Channel, Id, Status},
    ui,
};

/// Focus mode lasts at most this long; the server refuses more.
const DAY: i64 = 24 * 60 * 60;
/// How long the notice before joining keeps its button locked.
const NOTICE_SECONDS: u64 = 3;
/// The countdown choices, in seconds; 0 is "Other".
const LENGTHS: [i64; 5] = [30 * 60, 60 * 60, 150 * 60, 8 * 60 * 60, 0];

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// A countdown.
    For,
    /// A clock time within the next 24 hours.
    Until,
}

pub enum Dialog {
    Start {
        server: usize,
        channel: Id,
        mode: Mode,
        length: i64,
        minutes: String,
        time: String,
        error: Option<&'static str>,
    },
    /// Shown before joining a focus channel's call.
    Notice {
        server: usize,
        channel: Id,
        shown: Instant,
    },
}

impl Dialog {
    pub fn start(server: usize, channel: Id) -> Self {
        Self::Start {
            server,
            channel,
            mode: Mode::For,
            length: LENGTHS[0],
            minutes: String::new(),
            time: String::new(),
            error: None,
        }
    }
}

/// The badge text's font, as `ui::badge` draws it.
fn badge_font() -> FontId {
    FontId::new(ui::TEXT_CAPTION - 1.0, ui::bold_family())
}

/// The "Focus" badge's size, as `ui::badge` draws it.
fn badge_size(ui: &Ui) -> Vec2 {
    let text = t!("focus.badge").to_owned();
    let galley = ui
        .painter()
        .layout_no_wrap(text, badge_font(), Color32::WHITE);
    galley.size() + Vec2::new(10.0, 2.0)
}

/// The room the "Focus" badge takes after a channel's name.
pub fn badge_room(ui: &Ui) -> f32 {
    badge_size(ui).x + ui::SPACE_S
}

/// The "Focus" badge right after `name` in a channel `row` drawn by
/// `ui::nav_row_trailing`, which kept `trailing` points free (the badge's room included).
pub fn badge(ui: &mut Ui, row: Rect, name: &str, selected: bool, trailing: f32) {
    let font = FontId::new(ui::TEXT_BODY, crate::emoji::family(selected));
    let name = ui
        .painter()
        .layout_no_wrap(name.to_owned(), font, Color32::WHITE);
    let size = badge_size(ui);
    let width = name.size().x.min((row.width() - 44.0 - trailing).max(0.0));
    let left = row.left() + 34.0 + width + ui::SPACE_S;
    let rect = Rect::from_min_size(egui::pos2(left, row.center().y - size.y / 2.0), size);
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    ui::badge(&mut child, t!("focus.badge"), ui::accent(ui).base).on_hover_text(t!("focus.help"));
}

/// "1h 5m", "28m 32s" or "9s".
fn remaining(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        t!("focus.hours_minutes", hours = hours, minutes = minutes)
    } else if minutes > 0 {
        t!(
            "focus.minutes_seconds",
            minutes = minutes,
            seconds = seconds
        )
    } else {
        t!("focus.seconds", seconds = seconds)
    }
}

/// The bar under a focus channel's title, counting down to its end.
pub fn banner(ui: &mut Ui, channel: Option<&Channel>) {
    let now = unix_now();
    let Some(until) = channel.and_then(|channel| channel.focus(now)) else {
        return;
    };
    ui.ctx().request_repaint_after(Duration::from_secs(1));
    let text = t!("focus.banner", time = remaining(until - now));
    Frame::new()
        .fill(ui::accent(ui).base)
        .corner_radius(ui::RADIUS_S)
        .inner_margin(Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                let text = RichText::new(text).family(ui::bold_family());
                ui.label(text.color(ui::ON_FILL))
                    .on_hover_text(t!("focus.help"));
            });
        });
}

/// Before joining a focus channel's call, show the notice instead. True when it did.
pub fn ask_first(home: &mut Home, server: usize, channel: Id) -> bool {
    let on = (home.servers.get(server))
        .and_then(|server| server.channel(&channel))
        .is_some_and(|channel| channel.focus(unix_now()).is_some());
    if on {
        home.servers[server].open = Some(channel);
        home.focus = Some(Dialog::Notice {
            server,
            channel,
            shown: Instant::now(),
        });
    }
    on
}

/// "Focus mode…" and, while it is on, "End focus mode" in a channel's menu.
/// `Some(true)` opens the start dialog, `Some(false)` ends focus mode.
pub fn menu(m: &mut ui::Menu, channel: &Channel) -> Option<bool> {
    let mut picked = None;
    if m.item("🎯", t!("focus.start")).clicked() {
        picked = Some(true);
    }
    if channel.focus(unix_now()).is_some() && m.item("⏹", t!("focus.end")).clicked() {
        picked = Some(false);
    }
    picked
}

/// The next `time` on the clock after `now`, in seconds from now: today, or else tomorrow.
fn until_time<Tz: TimeZone>(time: NaiveTime, now: &DateTime<Tz>) -> Option<i64> {
    let zone = now.timezone();
    // A time the clocks skip has none; one they repeat means the first.
    let today = zone
        .from_local_datetime(&now.date_naive().and_time(time))
        .earliest();
    let at = match today.filter(|at| at > now) {
        Some(at) => at,
        None => {
            let tomorrow = now.date_naive().succ_opt()?.and_time(time);
            zone.from_local_datetime(&tomorrow).earliest()?
        }
    };
    Some((at.timestamp() - now.timestamp()).min(DAY + 1))
}

/// How many seconds focus mode lasts with these choices, or why it can't start.
fn length(mode: Mode, length: i64, minutes: &str, time: &str) -> Result<i64, &'static str> {
    let seconds = match mode {
        Mode::For if length > 0 => length,
        Mode::For => {
            let minutes = (minutes.trim().parse::<i64>().ok())
                .filter(|minutes| (1..=DAY / 60).contains(minutes))
                .ok_or(t!("focus.bad_minutes"))?;
            minutes * 60
        }
        Mode::Until => {
            let time = parse_time(time).ok_or(t!("events.bad_time"))?;
            until_time(time, &Local::now()).ok_or(t!("events.bad_time"))?
        }
    };
    match seconds {
        1..=DAY => Ok(seconds),
        _ => Err(t!("focus.too_long")),
    }
}

/// The open focus dialog, if any.
pub fn dialog(ctx: &egui::Context, home: &mut Home) {
    let Some(dialog) = &mut home.focus else {
        return;
    };
    let mut close = false;
    let mut join = None;
    let mut send = None;
    match dialog {
        Dialog::Start {
            server,
            channel,
            mode,
            length: choice,
            minutes,
            time,
            error,
        } => {
            let Some(target) = home.servers.get(*server) else {
                home.focus = None;
                return;
            };
            let (index, channel) = (*server, *channel);
            let ((), dismissed) = ui::dialog(ctx, "focus-start", t!("focus.start_title"), |ui| {
                ui.label(t!("focus.start_help"));
                ui.add_space(ui::SPACE_M);
                let modes = [
                    (Mode::For, t!("focus.for"), ""),
                    (Mode::Until, t!("focus.until"), ""),
                ];
                if ui::segmented(ui, mode, &modes) {
                    *error = None;
                }
                ui.add_space(ui::SPACE_S);
                let mut submitted = false;
                match mode {
                    Mode::For => {
                        let labels = [
                            t!("focus.length_30m"),
                            t!("focus.length_1h"),
                            t!("focus.length_2h30m"),
                            t!("focus.length_8h"),
                            t!("focus.length_other"),
                        ];
                        let choices: Vec<_> = (LENGTHS.iter().zip(labels))
                            .map(|(length, label)| (*length, label, ""))
                            .collect();
                        ui::segmented(ui, choice, &choices);
                        if *choice == 0 {
                            ui.add_space(ui::SPACE_S);
                            ui.label(ui::bold(t!("focus.minutes")));
                            let field = ui.add(
                                ui::field(minutes)
                                    .char_limit(4)
                                    .hint_text("45")
                                    .desired_width(80.0),
                            );
                            submitted = field.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter));
                        }
                    }
                    Mode::Until => {
                        ui.label(ui::bold(t!("focus.end_time")));
                        let field = ui.add(
                            ui::field(time)
                                .char_limit(5)
                                .hint_text("20:00")
                                .desired_width(80.0),
                        );
                        submitted = field.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter));
                        if let Some(at) = parse_time(time)
                            .and_then(|at| until_time(at, &Local::now()))
                            .filter(|seconds| *seconds <= DAY)
                        {
                            let end = Local::now() + chrono::Duration::seconds(at);
                            let end = crate::i18n::time(end, "time.day_clock");
                            ui.label(ui::caption(t!("focus.ends_at", time = end)));
                        }
                        ui.label(ui::caption(t!("focus.until_help")));
                    }
                }
                ui::error_text(ui, *error);
                let online = target.status == Status::Online;
                ui::footer(ui, |ui| {
                    let start = ui
                        .add_enabled_ui(online, |ui| ui::primary(ui, t!("focus.start_button")))
                        .inner;
                    if (start.clicked() || submitted) && online {
                        match length(*mode, *choice, minutes, time) {
                            Ok(seconds) => {
                                send = Some((index, Command::SetFocus(channel, seconds)))
                            }
                            Err(problem) => *error = Some(problem),
                        }
                    }
                    close |= ui::secondary(ui, t!("common.cancel")).clicked();
                });
            });
            close |= dismissed;
        }
        Dialog::Notice {
            server,
            channel,
            shown,
        } => {
            let until = (home.servers.get(*server))
                .and_then(|server| server.channel(channel))
                .and_then(|channel| channel.focus(unix_now()));
            // Focus mode ended meanwhile, or the channel is gone: nothing to warn about.
            let Some(until) = until else {
                let (server, channel) = (*server, *channel);
                home.focus = None;
                if home
                    .servers
                    .get(server)
                    .is_some_and(|s| s.channel(&channel).is_some())
                {
                    call::join_now(home, server, channel);
                }
                return;
            };
            let left = NOTICE_SECONDS.saturating_sub(shown.elapsed().as_secs());
            let tick = if left > 0 { 200 } else { 1000 };
            ctx.request_repaint_after(Duration::from_millis(tick));
            let ((), dismissed) = ui::dialog(ctx, "focus-notice", t!("focus.notice_title"), |ui| {
                ui.label(t!("focus.help"));
                ui.add_space(ui::SPACE_S);
                let ends = t!("focus.ends_in", time = remaining(until - unix_now()));
                ui.label(ui::caption(ends));
                ui::footer(ui, |ui| {
                    let label = if left > 0 {
                        t!("focus.join_wait", seconds = left)
                    } else {
                        t!("focus.join").to_owned()
                    };
                    let accept = ui
                        .add_enabled_ui(left == 0, |ui| ui::primary(ui, &label))
                        .inner;
                    if accept.clicked() && left == 0 {
                        join = Some((*server, *channel));
                    }
                    close |= ui::secondary(ui, t!("common.cancel")).clicked();
                });
            });
            close |= dismissed;
        }
    }
    if let Some((server, command)) = send {
        home.servers[server].send(command);
        close = true;
    }
    if close || join.is_some() {
        home.focus = None;
    }
    if let Some((server, channel)) = join {
        call::join_now(home, server, channel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn a_time_means_its_next_turn_within_a_day() {
        let now = Utc.with_ymd_and_hms(2026, 11, 23, 22, 30, 0).unwrap();
        let at =
            |hours, minutes| until_time(NaiveTime::from_hms_opt(hours, minutes, 0).unwrap(), &now);
        assert_eq!(at(23, 0), Some(30 * 60));
        // 01:24 has passed today, so it means tomorrow night.
        assert_eq!(at(1, 24), Some(2 * 3600 + 54 * 60));
        // The same minute is a whole day away.
        assert_eq!(at(22, 30), Some(DAY));
    }

    #[test]
    fn lengths_stay_within_a_day() {
        assert_eq!(length(Mode::For, 1800, "", ""), Ok(1800));
        assert_eq!(length(Mode::For, 0, " 45 ", ""), Ok(45 * 60));
        assert_eq!(length(Mode::For, 0, "1440", ""), Ok(DAY));
        assert!(length(Mode::For, 0, "1441", "").is_err());
        assert!(length(Mode::For, 0, "0", "").is_err());
        assert!(length(Mode::For, 0, "soon", "").is_err());
        assert!(length(Mode::Until, 0, "", "25:00").is_err());
    }

    #[test]
    fn time_left_reads_short() {
        let minutes_seconds = t!("focus.minutes_seconds", minutes = 28, seconds = 32);
        assert_eq!(remaining(28 * 60 + 32), minutes_seconds);
        let hours_minutes = t!("focus.hours_minutes", hours = 1, minutes = 5);
        assert_eq!(remaining(3600 + 5 * 60 + 9), hours_minutes);
        assert_eq!(remaining(-3), t!("focus.seconds", seconds = 0));
    }
}
