//! Server settings > Rate limits: how fast members may post, upload and so on.

use eframe::egui::Ui;
use rusttalk_core::auth::{RateLimitPreset, RateLimits};

use crate::ui;

pub(super) struct Form {
    /// What the server has; `None` when it is too old to change it.
    saved: Option<RateLimits>,
    picked: RateLimitPreset,
}

/// Each preset with its icon, name and line of help.
fn presets() -> [(RateLimitPreset, &'static str, &'static str, &'static str); 3] {
    [
        (
            RateLimitPreset::Relaxed,
            "🐢",
            t!("admin.rate_relaxed"),
            t!("admin.rate_relaxed_help"),
        ),
        (
            RateLimitPreset::Standard,
            "⚖",
            t!("admin.rate_standard"),
            t!("admin.rate_standard_help"),
        ),
        (
            RateLimitPreset::Strict,
            "🛡",
            t!("admin.rate_strict"),
            t!("admin.rate_strict_help"),
        ),
    ]
}

impl Form {
    pub(super) fn new(saved: Option<RateLimits>) -> Self {
        Self {
            saved,
            picked: saved.map(|saved| saved.preset).unwrap_or_default(),
        }
    }

    pub(super) fn unsaved(&self) -> bool {
        self.saved.is_some_and(|saved| saved.preset != self.picked)
    }

    /// Take `saved` as what's saved, keeping what was picked.
    pub(super) fn saved(&mut self, saved: Option<RateLimits>) {
        self.saved = saved;
    }

    /// The page; a preset to save.
    pub(super) fn show(&mut self, ui: &mut Ui, editable: bool) -> Option<RateLimitPreset> {
        let Some(saved) = self.saved else {
            ui.weak(t!("admin.history_unsupported"));
            return None;
        };
        ui.label(t!("admin.rate_help"));
        ui.add_space(ui::SPACE_S);
        let editable_preset = |preset| editable && saved.ceiling.is_none_or(|cap| preset <= cap);
        // One under another: three side by side don't fit a phone.
        let width = ui.available_width().min(360.0);
        for (preset, icon, title, help) in presets() {
            let card = ui.add_enabled_ui(editable_preset(preset), |ui| {
                ui::choice_card_sized(ui, width, self.picked == preset, icon, title, help)
            });
            if card.inner.clicked() {
                self.picked = preset;
            }
        }
        if let Some(cap) = saved.ceiling {
            let name = (presets().into_iter())
                .find(|(preset, ..)| *preset == cap)
                .map_or("", |(_, _, title, _)| title);
            ui.add_space(ui::SPACE_S);
            ui.weak(t!(
                "admin.rate_host_limit",
                preset = name,
                setting = "RUSTTALK_RATE_LIMITS"
            ));
        }
        if !editable {
            return None;
        }
        ui.add_space(ui::SPACE_S);
        let save = ui
            .add_enabled_ui(self.picked != saved.preset, |ui| {
                ui::primary(ui, t!("common.save"))
            })
            .inner
            .clicked();
        save.then_some(self.picked)
    }
}
