//! Server settings > Statistics: counts and sizes the server shares, refreshed every 2 s.

use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use eframe::egui::{self, Color32, RichText, Sense, Ui, Vec2};
use rusttalk_core::auth::{ServerStats, StatsDirection};

use crate::{files::size, net::Admin, ui};

/// How often the page asks again while it is open.
const REFRESH: Duration = Duration::from_secs(2);
/// The narrowest a tile gets before the row holds one fewer.
const TILE_WIDTH: f32 = 160.0;
/// Room for a detail line that wraps once, so tiles in a row match.
const TILE_HEIGHT: f32 = 84.0;

#[derive(Default)]
pub(super) struct View {
    stats: Option<ServerStats>,
    error: Option<String>,
    asked: Option<Instant>,
}

impl View {
    pub(super) fn apply(&mut self, update: Result<ServerStats, String>) {
        match update {
            Ok(stats) => (self.stats, self.error) = (Some(stats), None),
            Err(text) => self.error = Some(text),
        }
    }

    /// Ask for fresh numbers once `REFRESH` passed, and show what's known.
    pub(super) fn show(&mut self, ui: &mut Ui) -> Option<Admin> {
        let due = self.asked.is_none_or(|at| at.elapsed() >= REFRESH);
        ui.ctx().request_repaint_after(REFRESH);
        ui::page_section(ui, t!("admin.stats_now"));
        ui.label(ui::caption(t!("admin.stats_help")));
        ui.add_space(ui::SPACE_S);
        if let Some(text) = &self.error {
            ui::error_text(ui, Some(text));
        }
        match &self.stats {
            Some(stats) => {
                tiles(ui, stats);
                by_kind(ui, stats);
            }
            None if self.error.is_none() => {
                ui.spinner();
            }
            None => {}
        }
        due.then(|| {
            self.asked = Some(Instant::now());
            Admin::LoadStats
        })
    }
}

/// The API's traffic plus LiveKit's, when it answered.
fn total(stats: &ServerStats) -> StatsDirection {
    let control = stats.traffic.control;
    let media = stats.traffic.media.unwrap_or_default();
    StatsDirection {
        received: control.received.saturating_add(media.received),
        sent: control.sent.saturating_add(media.sent),
        received_per_sec: control
            .received_per_sec
            .saturating_add(media.received_per_sec),
        sent_per_sec: control.sent_per_sec.saturating_add(media.sent_per_sec),
    }
}

fn tiles(ui: &mut Ui, stats: &ServerStats) {
    let traffic = total(stats);
    let mut people = t!("admin.stats_members", members = stats.members.max(0) as u64);
    if stats.peak.count > 0 {
        let peak = t!(
            "admin.stats_peak",
            people = stats.peak.count,
            at = clock(stats.peak.at)
        );
        people = format!("{people} · {peak}");
    }
    let storage = t!(
        "admin.stats_storage_split",
        database = size(stats.storage.database),
        files = size(stats.storage.files)
    );
    let mut list = vec![(
        t!("admin.stats_connected").to_owned(),
        stats.connected.to_string(),
        people,
    )];
    if let Some(voice) = stats.voice {
        list.extend([
            (
                t!("admin.stats_in_voice").to_owned(),
                voice.people.to_string(),
                t!("admin.stats_voice_channels", count = voice.channels),
            ),
            (
                t!("admin.stats_audio").to_owned(),
                voice.audio.to_string(),
                t!("admin.stats_audio_help").to_owned(),
            ),
            (
                t!("admin.stats_video").to_owned(),
                voice.camera.saturating_add(voice.screen).to_string(),
                t!(
                    "admin.stats_video_split",
                    cameras = voice.camera,
                    screens = voice.screen
                ),
            ),
        ]);
    }
    list.extend([
        (
            t!("admin.stats_logins").to_owned(),
            stats.logins.to_string(),
            t!("admin.stats_since_start").to_owned(),
        ),
        (
            t!("admin.stats_messages").to_owned(),
            stats.messages.to_string(),
            t!("admin.stats_since_start").to_owned(),
        ),
        (
            t!("admin.stats_incoming").to_owned(),
            per_second(traffic.received_per_sec),
            t!("admin.stats_total", total = size(traffic.received)),
        ),
        (
            t!("admin.stats_outgoing").to_owned(),
            per_second(traffic.sent_per_sec),
            t!("admin.stats_total", total = size(traffic.sent)),
        ),
        (
            t!("admin.stats_storage").to_owned(),
            size(stats.storage.database.saturating_add(stats.storage.files)),
            storage,
        ),
        (
            t!("admin.stats_uptime").to_owned(),
            uptime(stats.uptime_secs),
            t!("admin.stats_version", version = stats.version),
        ),
    ]);
    if let Some(memory) = stats.memory {
        list.push((
            t!("admin.stats_memory").to_owned(),
            size(memory),
            t!("admin.stats_memory_help").to_owned(),
        ));
    }
    let gap = ui::SPACE_S;
    let available = ui.available_width();
    let columns = ((available + gap) / (TILE_WIDTH + gap)).floor().max(1.0);
    let width = (available - gap * (columns - 1.0)) / columns;
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(gap, gap);
        for row in list.chunks(columns as usize) {
            ui.horizontal(|ui| {
                for (label, value, detail) in row {
                    tile(ui, width, label, value, detail);
                }
            });
        }
    });
}

/// Traffic since start split into voice and video and the rest, in and out.
fn by_kind(ui: &mut Ui, stats: &ServerStats) {
    ui::page_section(ui, t!("admin.stats_by_kind"));
    let control = stats.traffic.control;
    let Some(media) = stats.traffic.media else {
        ui.label(ui::caption(t!("admin.stats_no_media")));
        return;
    };
    let media_color = ui::accent(ui).base;
    let control_color = ui::colors(ui).muted;
    for (label, media, control) in [
        (t!("admin.stats_incoming"), media.received, control.received),
        (t!("admin.stats_outgoing"), media.sent, control.sent),
    ] {
        ui::caption_label(ui, label);
        bar(ui, &[(media, media_color), (control, control_color)]);
        ui.horizontal_wrapped(|ui| {
            key(
                ui,
                media_color,
                &t!("admin.stats_media", size = size(media)),
            );
            key(
                ui,
                control_color,
                &t!("admin.stats_control", size = size(control)),
            );
        });
        ui.add_space(ui::SPACE_S);
    }
    ui.label(ui::caption(t!("admin.stats_media_scope")));
}

/// A bar split in proportion to `parts`.
fn bar(ui: &mut Ui, parts: &[(u64, Color32)]) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 12.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, ui::RADIUS_S, ui::colors(ui).hover);
    let sum: u64 = parts.iter().map(|(bytes, _)| *bytes).sum();
    if sum == 0 {
        return;
    }
    let mut left = rect.left();
    for (bytes, color) in parts {
        let width = rect.width() * (*bytes as f64 / sum as f64) as f32;
        if width <= 0.0 {
            continue;
        }
        let part = egui::Rect::from_min_size(
            egui::pos2(left, rect.top()),
            Vec2::new(width, rect.height()),
        );
        painter.rect_filled(part, ui::RADIUS_S, *color);
        left += width;
    }
}

fn key(ui: &mut Ui, color: Color32, text: &str) {
    let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(dot.center(), 4.0, color);
    ui.label(ui::caption(text));
    ui.add_space(ui::SPACE_S);
}

fn tile(ui: &mut Ui, width: f32, label: &str, value: &str, detail: &str) {
    let inner = (width - 2.0 * ui::SPACE_L).max(0.0);
    ui::card_frame(ui).show(ui, |ui| {
        ui.set_width(inner);
        ui.set_min_height(TILE_HEIGHT);
        ui.vertical(|ui| {
            ui::caption_label(ui, label);
            let strong = ui::colors(ui).strong;
            ui.label(RichText::new(value).size(ui::TEXT_SECTION).color(strong));
            ui.add(egui::Label::new(ui::caption(detail)).wrap());
        });
    });
}

fn per_second(bytes: u64) -> String {
    t!("admin.stats_per_second", size = size(bytes))
}

fn clock(at: i64) -> String {
    DateTime::from_timestamp(at, 0).map_or_else(String::new, |at| {
        crate::i18n::time(at.with_timezone(&Local), "time.day_clock")
    })
}

fn uptime(secs: u64) -> String {
    let (days, hours, minutes) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    if days > 0 {
        t!(
            "admin.stats_uptime_days",
            days = days,
            hours = hours,
            minutes = minutes
        )
    } else {
        t!("admin.stats_uptime_hours", hours = hours, minutes = minutes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_reads_in_days_hours_and_minutes() {
        assert_eq!(uptime(59), "0 h 0 min");
        assert_eq!(uptime(3 * 3600 + 7 * 60), "3 h 7 min");
        assert_eq!(uptime(2 * 86_400 + 3600 + 60), "2 d 1 h 1 min");
    }
}
