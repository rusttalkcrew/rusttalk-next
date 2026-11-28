//! Statistics > History: traffic and people over the last 5 minutes to 24 hours.

use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Shape, Stroke, Ui, Vec2, pos2};
use rusttalk_core::auth::{StatsHistory, StatsPoint};

use super::{key, per_second};
use crate::ui;

/// The server takes a point every 10 s, so asking more often shows nothing new.
const REFRESH: Duration = Duration::from_secs(10);
const CHART_HEIGHT: f32 = 120.0;

pub(super) struct History {
    range: i64,
    data: Option<StatsHistory>,
    error: Option<String>,
    asked: Option<Instant>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            range: 3600,
            data: None,
            error: None,
            asked: None,
        }
    }
}

impl History {
    pub(super) fn apply(&mut self, update: Result<StatsHistory, String>) {
        match update {
            // An answer for a range picked before the current one is dropped.
            Ok(data) if data.range == self.range => (self.data, self.error) = (Some(data), None),
            Ok(_) => {}
            Err(text) => self.error = Some(text),
        }
    }

    /// The range to load, when due.
    pub(super) fn due(&self) -> bool {
        self.asked.is_none_or(|at| at.elapsed() >= REFRESH)
    }

    pub(super) fn asking(&mut self) -> i64 {
        self.asked = Some(Instant::now());
        self.range
    }

    pub(super) fn show(&mut self, ui: &mut Ui) {
        ui::page_section(ui, t!("admin.stats_history"));
        let choices = [
            (300, t!("admin.stats_range_5_min"), ""),
            (3600, t!("admin.stats_range_1_h"), ""),
            (21_600, t!("admin.stats_range_6_h"), ""),
            (86_400, t!("admin.stats_range_24_h"), ""),
        ];
        if ui::segmented(ui, &mut self.range, &choices) {
            (self.data, self.asked) = (None, None);
        }
        ui.add_space(ui::SPACE_S);
        if let Some(text) = &self.error {
            ui::error_text(ui, Some(text));
        }
        let Some(data) = &self.data else {
            if self.error.is_none() {
                ui.spinner();
            }
            return;
        };
        if data.points.len() < 2 {
            ui.label(ui::caption(t!("admin.stats_history_empty")));
        } else {
            let accent = ui::accent(ui).base;
            let second = ui::colors(ui).success;
            let traffic = [
                (
                    t!("admin.stats_incoming"),
                    accent,
                    values(data, |p| p.received_per_sec),
                ),
                (
                    t!("admin.stats_outgoing"),
                    second,
                    values(data, |p| p.sent_per_sec),
                ),
            ];
            ui::caption_label(ui, t!("admin.stats_traffic_chart"));
            chart(ui, data, &traffic, |value| per_second(value as u64));
            ui.add_space(ui::SPACE_S);
            let people = [
                (
                    t!("admin.stats_connected"),
                    accent,
                    values(data, |p| Some(p.connected)),
                ),
                (
                    t!("admin.stats_in_voice"),
                    second,
                    values(data, |p| p.voice),
                ),
            ];
            ui::caption_label(ui, t!("admin.stats_people_chart"));
            chart(ui, data, &people, |value| format!("{value:.0}"));
        }
        ui.add_space(ui::SPACE_S);
        ui.label(ui::caption(t!("admin.stats_history_help")));
    }
}

type Series = Vec<(i64, Option<f64>)>;

fn values(data: &StatsHistory, value: impl Fn(&StatsPoint) -> Option<u64>) -> Series {
    (data.points.iter())
        .map(|point| (point.at, value(point).map(|v| v as f64)))
        .collect()
}

/// Lines over the range, broken where a step has no value or no point.
fn chart(
    ui: &mut Ui,
    data: &StatsHistory,
    lines: &[(&str, Color32, Series)],
    label: impl Fn(f64) -> String,
) {
    let end = data.points.last().map_or(0, |point| point.at);
    let start = end - data.range;
    let top = (lines.iter())
        .flat_map(|(_, _, series)| series.iter().filter_map(|(_, value)| *value))
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let c = ui::colors(ui);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), CHART_HEIGHT),
        Sense::hover(),
    );
    let painter = ui.painter_at(rect.expand(1.0));
    painter.rect_filled(rect, ui::RADIUS_S, ui.visuals().extreme_bg_color);
    let plot = rect.shrink2(Vec2::new(ui::SPACE_S, ui::SPACE_S));
    let x = |at: i64| plot.left() + plot.width() * ((at - start) as f32 / data.range as f32);
    let y = |value: f64| plot.bottom() - plot.height() * (value / top) as f32;
    painter.hline(plot.x_range(), plot.bottom(), Stroke::new(1.0, c.border));
    for (_, color, series) in lines {
        let mut run: Vec<Pos2> = Vec::new();
        let mut last_at = None;
        for (at, value) in series {
            let gap = last_at.is_some_and(|last| at - last > data.step * 3 / 2);
            match value {
                Some(value) if !gap || run.is_empty() => run.push(pos2(x(*at), y(*value))),
                Some(value) => {
                    draw(&painter, std::mem::take(&mut run), *color);
                    run.push(pos2(x(*at), y(*value)));
                }
                None => draw(&painter, std::mem::take(&mut run), *color),
            }
            last_at = Some(*at);
        }
        draw(&painter, run, *color);
    }
    let font = egui::FontId::proportional(ui::TEXT_CAPTION);
    painter.text(
        plot.left_top(),
        egui::Align2::LEFT_TOP,
        label(top),
        font.clone(),
        c.muted,
    );
    if let Some(hover) = response.hover_pos()
        && let Some(text) = hover_values(ui, plot, data, lines, hover, &label)
    {
        response.show_tooltip_text(text);
    }
    let time = if data.range > 3600 {
        "time.day_clock"
    } else {
        "time.clock"
    };
    ui.horizontal(|ui| {
        ui.label(ui::caption(clock(start, time)));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(ui::caption(clock(end, time)));
        });
    });
    ui.horizontal_wrapped(|ui| {
        for (name, color, _) in lines {
            key(ui, *color, name);
        }
    });
}

fn draw(painter: &egui::Painter, points: Vec<Pos2>, color: Color32) {
    match points.len() {
        0 => {}
        1 => {
            painter.circle_filled(points[0], 1.5, color);
        }
        _ => {
            painter.add(Shape::line(points, Stroke::new(2.0, color)));
        }
    }
}

/// The values at the step nearest the pointer, with a line marking it.
fn hover_values(
    ui: &Ui,
    plot: Rect,
    data: &StatsHistory,
    lines: &[(&str, Color32, Series)],
    hover: Pos2,
    label: &impl Fn(f64) -> String,
) -> Option<String> {
    let end = data.points.last().map_or(0, |point| point.at);
    let start = end - data.range;
    let at = start + ((hover.x - plot.left()) / plot.width() * data.range as f32) as i64;
    let index = (0..data.points.len()).min_by_key(|i| (data.points[*i].at - at).abs())?;
    let point_at = data.points[index].at;
    let x = plot.left() + plot.width() * ((point_at - start) as f32 / data.range as f32);
    ui.painter()
        .vline(x, plot.y_range(), Stroke::new(1.0, ui::colors(ui).border));
    let mut text = clock(point_at, "time.day_clock");
    for (name, _, series) in lines {
        let value = series[index].1.map_or_else(|| "–".to_owned(), label);
        text.push_str(&format!("\n{name}: {value}"));
    }
    Some(text)
}

fn clock(at: i64, key: &'static str) -> String {
    DateTime::from_timestamp(at, 0).map_or_else(String::new, |at| {
        crate::i18n::time(at.with_timezone(&Local), key)
    })
}
