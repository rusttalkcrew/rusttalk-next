//! Server settings > Statistics: counts and sizes the server shares, refreshed every 2 s.

use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use eframe::egui::{self, RichText, Ui};
use rusttalk_core::auth::ServerStats;

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
            Some(stats) => tiles(ui, stats),
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

fn tiles(ui: &mut Ui, stats: &ServerStats) {
    let control = stats.traffic.control;
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
    let mut list = vec![
        (
            t!("admin.stats_connected").to_owned(),
            stats.connected.to_string(),
            people,
        ),
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
            per_second(control.received_per_sec),
            t!("admin.stats_total", total = size(control.received)),
        ),
        (
            t!("admin.stats_outgoing").to_owned(),
            per_second(control.sent_per_sec),
            t!("admin.stats_total", total = size(control.sent)),
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
    ];
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
    ui.add_space(gap);
    ui.label(ui::caption(t!("admin.stats_traffic_scope")));
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
