//! Server settings > Message history: how many messages each channel keeps.

use eframe::egui::{self, DragValue, Ui, accesskit::Role};
use rusttalk_core::auth::MessageHistory;

use crate::ui;

/// What a server keeps by default, offered when someone stops keeping everything.
const DEFAULT: i64 = 10_000;

pub(super) struct Form {
    /// What the server has; `None` when it is too old to change it.
    saved: Option<MessageHistory>,
    keep_all: bool,
    count: i64,
    /// A lower number waiting for "Delete older messages" to be confirmed.
    confirming: Option<i64>,
}

impl Form {
    pub(super) fn new(saved: Option<MessageHistory>) -> Self {
        let keep = saved.map_or(0, |saved| saved.keep);
        Self {
            saved,
            keep_all: keep == 0,
            count: if keep == 0 { DEFAULT } else { keep },
            confirming: None,
        }
    }

    fn proposed(&self) -> i64 {
        if self.keep_all { 0 } else { self.count }
    }

    pub(super) fn unsaved(&self) -> bool {
        self.saved
            .is_some_and(|saved| saved.keep != self.proposed())
    }

    /// Take `saved` as what's saved, keeping what was typed.
    pub(super) fn saved(&mut self, saved: Option<MessageHistory>) {
        self.saved = saved;
        self.confirming = None;
    }

    /// The page; a number to save once it needs no confirmation.
    pub(super) fn show(&mut self, ui: &mut Ui, editable: bool) -> Option<i64> {
        let Some(saved) = self.saved else {
            ui.weak(t!("admin.history_unsupported"));
            return None;
        };
        if !editable {
            let value = match saved.keep {
                0 => t!("admin.history_all").to_owned(),
                keep => keep.to_string(),
            };
            ui::setting_row(ui, t!("admin.history_count"), "", |ui| {
                ui.label(value);
            });
            return None;
        }
        // A host cap rules out keeping everything.
        if saved.ceiling.is_none() {
            ui::toggle_row(
                ui,
                &mut self.keep_all,
                t!("admin.history_keep_all"),
                t!("admin.history_keep_all_help"),
            );
        }
        if !self.keep_all || saved.ceiling.is_some() {
            self.keep_all = false;
            let help = match saved.ceiling {
                Some(limit) => format!(
                    "{} {}",
                    t!("admin.history_count_help"),
                    t!(
                        "admin.history_host_limit",
                        limit = limit,
                        setting = "RUSTTALK_MESSAGE_RETAIN"
                    )
                ),
                None => t!("admin.history_count_help").to_owned(),
            };
            ui::setting_row(ui, t!("admin.history_count"), &help, |ui| {
                ui.add(
                    DragValue::new(&mut self.count).range(1..=saved.ceiling.unwrap_or(i64::MAX)),
                )
                .widget_info(|| {
                    egui::WidgetInfo::labeled(Role::SpinButton, true, t!("admin.history_count"))
                });
            });
        }
        let next = self.proposed();
        let save = ui
            .add_enabled_ui(next != saved.keep, |ui| ui::primary(ui, t!("common.save")))
            .inner
            .clicked();
        if !save {
            return None;
        }
        if deletes(saved.keep, next) {
            self.confirming = Some(next);
            return None;
        }
        Some(next)
    }

    /// Ask before a lower number deletes history.
    pub(super) fn confirm(&mut self, ctx: &egui::Context) -> Option<i64> {
        let keep = self.confirming?;
        let mut request = None;
        let mut cancel = false;
        let ((), dismissed) = ui::dialog(
            ctx,
            "message-history-lower",
            t!("admin.history_confirm_title"),
            |ui| {
                ui.label(t!("admin.history_confirm", number = keep));
                ui::footer(ui, |ui| {
                    if ui::danger(ui, t!("admin.history_confirm_delete")).clicked() {
                        request = Some(keep);
                    }
                    cancel = ui::secondary(ui, t!("common.cancel")).clicked();
                });
            },
        );
        if request.is_some() || cancel || dismissed {
            self.confirming = None;
        }
        request
    }
}

/// Going from `saved` to `next` deletes messages channels already hold.
fn deletes(saved: i64, next: i64) -> bool {
    next != 0 && (saved == 0 || next < saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_lower_limit_asks_before_deleting() {
        assert!(deletes(0, 5_000));
        assert!(deletes(10_000, 9_999));
        assert!(!deletes(10_000, 0));
        assert!(!deletes(10_000, 20_000));
        assert!(!deletes(0, 0));
    }
}
