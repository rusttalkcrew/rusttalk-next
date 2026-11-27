//! Server settings > New members: how long the new member rules last after joining.

use eframe::egui::{self, DragValue, Ui, accesskit::Role};
use rusttalk_core::auth::NewMemberRules;

use crate::ui;

/// The preset lengths in seconds, 0 being off; anything else is a custom length.
const PRESETS: [i64; 4] = [0, 600, 3600, 86_400];
/// The choice that stands for a custom length.
const CUSTOM: i64 = -1;

pub(super) struct Form {
    /// What the server has; `None` when it is too old to have new member rules.
    saved: Option<NewMemberRules>,
    /// One of `PRESETS`, or `CUSTOM`.
    pick: i64,
    /// The custom length in minutes.
    minutes: i64,
}

impl Form {
    pub(super) fn new(saved: Option<NewMemberRules>) -> Self {
        let secs = saved.map_or(0, |saved| saved.secs);
        let preset = PRESETS.contains(&secs);
        Self {
            saved,
            pick: if preset { secs } else { CUSTOM },
            minutes: if preset { 60 } else { (secs + 59) / 60 },
        }
    }

    fn proposed(&self) -> i64 {
        if self.pick == CUSTOM {
            self.minutes * 60
        } else {
            self.pick
        }
    }

    pub(super) fn unsaved(&self) -> bool {
        self.saved
            .is_some_and(|saved| saved.secs != self.proposed())
    }

    /// Take `saved` as what's saved, keeping what was picked.
    pub(super) fn saved(&mut self, saved: Option<NewMemberRules>) {
        self.saved = saved;
    }

    /// The page; the length to save, in seconds.
    pub(super) fn show(&mut self, ui: &mut Ui) -> Option<i64> {
        let Some(saved) = self.saved else {
            ui.weak(t!("admin.new_members_unsupported"));
            return None;
        };
        ui.add(egui::Label::new(t!("admin.new_members_help")).wrap());
        ui.add_space(ui::SPACE_S);
        let choices = [
            (0, t!("admin.new_members_off"), ""),
            (600, t!("admin.new_members_10_min"), ""),
            (3600, t!("admin.new_members_1_h"), ""),
            (86_400, t!("admin.new_members_24_h"), ""),
            (CUSTOM, t!("admin.new_members_custom"), ""),
        ];
        ui::segmented(ui, &mut self.pick, &choices);
        if self.pick == CUSTOM {
            ui.add_space(ui::SPACE_S);
            let label = t!("admin.new_members_minutes");
            ui::setting_row(ui, label, t!("admin.new_members_minutes_help"), |ui| {
                ui.add(DragValue::new(&mut self.minutes).range(1..=NewMemberRules::MAX_SECS / 60))
                    .widget_info(|| egui::WidgetInfo::labeled(Role::SpinButton, true, label));
            });
        }
        ui.add_space(ui::SPACE_S);
        let next = self.proposed();
        ui.add_enabled_ui(next != saved.secs, |ui| ui::primary(ui, t!("common.save")))
            .inner
            .clicked()
            .then_some(next)
    }
}

/// "10 min", "1 h", or "3 h 20 min" for the audit log.
pub(super) fn label(secs: i64) -> String {
    if secs <= 0 {
        return t!("admin.new_members_off").into();
    }
    let minutes = (secs + 59) / 60;
    match (minutes / 60, minutes % 60) {
        (0, m) => t!("admin.new_members_length_m", minutes = m),
        (h, 0) => t!("admin.new_members_length_h", hours = h),
        (h, m) => t!("admin.new_members_length_hm", hours = h, minutes = m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_length_shows_as_its_preset_or_as_custom_minutes() {
        let form = |secs| Form::new(Some(NewMemberRules { secs }));
        assert_eq!((form(3600).pick, form(3600).proposed()), (3600, 3600));
        assert_eq!((form(5400).pick, form(5400).minutes), (CUSTOM, 90));
        assert!(!form(5400).unsaved());
        let mut custom = form(0);
        custom.pick = CUSTOM;
        assert_eq!(custom.proposed(), 3600);
        assert!(custom.unsaved());
        assert!(!Form::new(None).unsaved());
    }
}
