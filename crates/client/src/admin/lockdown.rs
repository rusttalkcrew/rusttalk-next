//! Server settings > Lockdown (#665): stop new people joining during a raid, by hand or
//! when many join at once, and lift it again.

use eframe::egui::{self, DragValue, Ui, accesskit::Role};

use crate::{server::Lockdown, ui};

/// What the server takes for a message, in characters.
const MESSAGE_CHARS: usize = 500;
/// Automatic lockdown's default, offered when someone turns it back on.
const DEFAULT_JOINS: i64 = 10;

pub(super) struct Form {
    /// What the server has.
    saved: Lockdown,
    message: String,
    auto: bool,
    joins: i64,
    minutes: i64,
}

impl Form {
    pub(super) fn new(saved: Lockdown) -> Self {
        Self {
            message: saved.message.clone().unwrap_or_default(),
            auto: saved.auto_joins > 0,
            joins: if saved.auto_joins > 0 {
                saved.auto_joins
            } else {
                DEFAULT_JOINS
            },
            minutes: (saved.auto_secs / 60).clamp(1, 60),
            saved,
        }
    }

    /// The settings as edited, locked or not.
    fn proposed(&self, locked: bool) -> Lockdown {
        let message = self.message.trim();
        Lockdown {
            locked,
            automatic: false,
            message: (!message.is_empty()).then(|| message.to_owned()),
            auto_joins: if self.auto { self.joins } else { 0 },
            auto_secs: self.minutes * 60,
        }
    }

    pub(super) fn unsaved(&self) -> bool {
        let saved = Lockdown {
            automatic: false,
            ..self.saved.clone()
        };
        let mut proposed = self.proposed(saved.locked);
        // While automatic lockdown stays off, its window doesn't matter.
        if proposed.auto_joins == 0 && saved.auto_joins == 0 {
            proposed.auto_secs = saved.auto_secs;
        }
        proposed != saved
    }

    /// Take `saved` as what's saved; edits waiting for Save stay.
    pub(super) fn sync(&mut self, saved: &Lockdown) {
        if self.saved == *saved {
            return;
        }
        if self.unsaved() {
            self.saved = saved.clone();
        } else {
            *self = Self::new(saved.clone());
        }
    }

    /// The page; the settings to save when a button was pressed.
    pub(super) fn show(&mut self, ui: &mut Ui) -> Option<Lockdown> {
        let mut request = None;
        ui::page_section(ui, t!("lockdown.title"));
        ui::card(ui, |ui| {
            let (title, help) = match (self.saved.locked, self.saved.automatic) {
                (false, _) => (t!("lockdown.off"), t!("lockdown.off_help")),
                (true, false) => (t!("lockdown.on"), t!("lockdown.on_help")),
                (true, true) => (t!("lockdown.on"), t!("lockdown.automatic_help")),
            };
            ui.label(ui::bold(title));
            ui.add(egui::Label::new(help).wrap());
            ui.add_space(ui::SPACE_S);
            let locked = self.saved.locked;
            let pressed = match locked {
                false => ui::danger(ui, t!("lockdown.lock")),
                true => ui::primary(ui, t!("lockdown.lift")),
            };
            if pressed.clicked() {
                // Unsaved edits go along: they are on screen next to the button.
                request = Some(self.proposed(!locked));
            }
        });
        ui::page_section(ui, t!("lockdown.message"));
        let label = ui.weak(t!("lockdown.message_help"));
        ui.add(
            ui::text_area(&mut self.message)
                .desired_rows(3)
                .char_limit(MESSAGE_CHARS)
                .desired_width(f32::INFINITY),
        )
        .labelled_by(label.id);
        ui::page_section(ui, t!("lockdown.auto"));
        ui::toggle_row(
            ui,
            &mut self.auto,
            t!("lockdown.auto_switch"),
            t!("lockdown.auto_help"),
        );
        if self.auto {
            number(ui, t!("lockdown.auto_joins"), &mut self.joins, 2..=1000);
            number(ui, t!("lockdown.auto_minutes"), &mut self.minutes, 1..=60);
        }
        let save = ui
            .add_enabled_ui(self.unsaved(), |ui| ui::primary(ui, t!("common.save")))
            .inner
            .clicked();
        if save {
            request = Some(self.proposed(self.saved.locked));
        }
        request
    }
}

fn number(ui: &mut Ui, label: &str, value: &mut i64, range: std::ops::RangeInclusive<i64>) {
    ui::setting_row(ui, label, "", |ui| {
        ui.add(DragValue::new(value).range(range))
            .widget_info(|| egui::WidgetInfo::labeled(Role::SpinButton, true, label));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(joins: i64, message: Option<&str>) -> Lockdown {
        Lockdown {
            locked: false,
            automatic: false,
            message: message.map(str::to_owned),
            auto_joins: joins,
            auto_secs: 120,
        }
    }

    #[test]
    fn locking_sends_the_edits_and_saving_needs_a_change() {
        let mut form = Form::new(saved(10, None));
        assert!(!form.unsaved());
        assert_eq!((form.joins, form.minutes), (10, 2));
        form.message = "  Back soon ".into();
        assert!(form.unsaved());
        let locked = form.proposed(true);
        assert_eq!(locked.message.as_deref(), Some("Back soon"));
        assert_eq!((locked.auto_joins, locked.auto_secs), (10, 120));
        form.auto = false;
        assert_eq!(form.proposed(false).auto_joins, 0);
        // Saved elsewhere: what's shown follows unless edits wait for Save.
        let mut form = Form::new(saved(0, None));
        assert!(!form.auto);
        form.sync(&saved(5, Some("Hi")));
        assert_eq!(
            (form.auto, form.joins, form.message.as_str()),
            (true, 5, "Hi")
        );
    }
}
