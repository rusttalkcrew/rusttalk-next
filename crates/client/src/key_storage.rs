//! The key storage screens: the first-start choice without a keychain, the password
//! unlock, the missing-keys help, and the section in Settings > Privacy. The start
//! screens use the big buttons and take Enter, for the Steam Deck's on-screen
//! keyboard and gamepad. Password work runs on its own thread (a spinner shows), and
//! typed passwords are wiped from egui's memory once a screen is done with them.

use std::time::Duration;

use eframe::egui::{Context, Id, Ui, text_edit::TextEditState};
use zeroize::Zeroizing;

use crate::{
    app::submitted,
    keystore::{Job, Keystore, Kind, RULES, Sealer, WrongPassword},
    ui::{self, error_text, primary_large, secondary_large},
};

/// Wrong passwords before the hint about the recovery words.
pub const HINT_AFTER: u32 = 3;
const FIELD_WIDTH: f32 = 320.0;
/// How often a waiting screen looks whether the password work is done.
pub const POLL: Duration = Duration::from_millis(50);

/// The password fields, by fixed IDs so their egui state can be removed.
fn field_id(name: &str) -> Id {
    Id::unique(("key storage password", name))
}

const FIELDS: [&str; 3] = ["unlock", "new", "repeat"];

/// Remove what egui keeps of the password fields: their text in the undo history.
pub fn forget(ctx: &Context) {
    for name in FIELDS {
        ctx.data_mut(|data| data.remove::<TextEditState>(field_id(name)));
    }
}

/// Reserve space up front so typing a phrase or password does not leave old allocations behind.
pub fn secret_input() -> Zeroizing<String> {
    Zeroizing::new(String::with_capacity(256))
}

/// A password typed twice.
pub struct NewPassword {
    pub password: Zeroizing<String>,
    pub repeat: Zeroizing<String>,
}

impl Default for NewPassword {
    fn default() -> Self {
        Self {
            password: secret_input(),
            repeat: secret_input(),
        }
    }
}

impl NewPassword {
    /// The fields, `width` wide; true when Enter was pressed in the last one.
    fn show(&mut self, ui: &mut Ui, width: f32) -> bool {
        let label = ui.label(t!("keys.password"));
        ui.add(
            ui::field(&mut *self.password)
                .id(field_id("new"))
                .password()
                .desired_width(width),
        )
        .labelled_by(label.id);
        let label = ui.label(t!("keys.repeat_password"));
        let repeat = ui
            .add(
                ui::field(&mut *self.repeat)
                    .id(field_id("repeat"))
                    .password()
                    .desired_width(width),
            )
            .labelled_by(label.id);
        submitted(ui, &repeat)
    }

    /// The password, or what's wrong with it.
    fn check(&self) -> Result<Zeroizing<String>, &'static str> {
        if self.password.is_empty() {
            Err(t!("keys.password_empty"))
        } else if *self.password != *self.repeat {
            Err(t!("keys.password_mismatch"))
        } else {
            Ok(self.password.clone())
        }
    }
}

pub enum Picked {
    File,
    /// The checked password, for the key to be derived from.
    Password(Zeroizing<String>),
}

/// First start without a keychain. Returns the choice once it's complete. Phones
/// offer only the password file here.
pub fn choose(
    ui: &mut Ui,
    password: &mut Option<NewPassword>,
    error: &mut Option<String>,
    busy: bool,
    keychain: bool,
) -> Option<Picked> {
    if RULES.phone() {
        let text = if !keychain && cfg!(target_os = "android") {
            t!("keys.no_android_keystore")
        } else if !keychain {
            t!("keys.no_keychain_phone")
        } else {
            t!("keys.password_only")
        };
        ui::step_title(ui, t!("keys.choose_title"), text);
        password.get_or_insert_with(NewPassword::default);
    } else {
        ui::step_title(ui, t!("keys.choose_title"), t!("keys.no_keychain"));
    }
    ui.add_enabled_ui(!busy, |ui| {
        if !RULES.phone() {
            if primary_large(ui, t!("keys.file")).clicked() {
                return Some(Picked::File);
            }
            ui.label(ui::caption(t!("keys.file_help")));
            ui.add_space(ui::SPACE_S);
            if secondary_large(ui, t!("keys.password_file")).clicked() && password.is_none() {
                *password = Some(NewPassword::default());
            }
            ui.label(ui::caption(t!("keys.password_file_help")));
        }
        let fields = password.as_mut()?;
        ui.add_space(ui::SPACE_L);
        let entered = fields.show(ui, f32::INFINITY);
        error_text(ui, error.as_deref());
        ui.add_space(ui::SPACE_S);
        let pressed = primary_large(ui, t!("keys.continue")).clicked();
        working(ui, busy);
        if !(pressed || entered) {
            return None;
        }
        match fields.check() {
            Ok(password) => Some(Picked::Password(password)),
            Err(problem) => {
                *error = Some(problem.into());
                None
            }
        }
    })
    .inner
}

/// A spinner under the button while the password work runs.
fn working(ui: &mut Ui, busy: bool) {
    if busy {
        ui.vertical_centered(|ui| ui.spinner());
    }
}

pub enum Unlock {
    Try,
    UseWords,
}

/// The password at start.
pub fn unlock(
    ui: &mut Ui,
    password: &mut Zeroizing<String>,
    error: Option<&str>,
    tries: u32,
    busy: bool,
) -> Option<Unlock> {
    ui::step_title(ui, t!("keys.unlock_title"), t!("keys.unlock_text"));
    ui.add_enabled_ui(!busy, |ui| {
        let label = ui.label(t!("keys.password"));
        let field = ui
            .add(
                ui::field(&mut **password)
                    .id(field_id("unlock"))
                    .password()
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(label.id);
        let entered = submitted(ui, &field);
        if !busy {
            ui::focus_when_none(ui, &field);
        }
        error_text(ui, error);
        ui.add_space(ui::SPACE_S);
        let pressed = primary_large(ui, t!("keys.unlock")).clicked();
        working(ui, busy);
        if pressed || entered {
            return Some(Unlock::Try);
        }
        if tries >= HINT_AFTER {
            ui.add_space(ui::SPACE_M);
            ui.label(t!("keys.forgot"));
            if secondary_large(ui, t!("keys.use_words")).clicked() {
                return Some(Unlock::UseWords);
            }
        }
        None
    })
    .inner
}

/// This profile's keys aren't found. True for "use recovery words".
pub fn missing(ui: &mut Ui, keychain: bool) -> bool {
    // The desktop-keychain help is about Steam Deck Desktop Mode, not phones.
    if keychain && !RULES.phone() {
        ui::step_title(ui, t!("keys.gone_title"), t!("keys.gone_text"));
    } else {
        ui::step_title(ui, t!("keys.missing_title"), t!("keys.missing_text"));
    }
    secondary_large(ui, t!("keys.use_words")).clicked()
}

/// What the Key storage section in Settings is doing.
#[derive(Default)]
pub struct Form {
    pick: Option<Kind>,
    password: NewPassword,
    error: Option<String>,
    done: bool,
    /// The key for a new password, being derived.
    busy: Option<Job<Sealer>>,
}

/// Leaving the Privacy page or closing Settings drops a half-typed password.
pub fn leave(ctx: &Context, form: &mut Form) {
    let typed = !form.password.password.is_empty() || !form.password.repeat.is_empty();
    if typed || form.busy.is_some() || form.pick.is_some() {
        *form = Form::default();
        forget(ctx);
    }
}

/// Settings > Privacy > Key storage: switch where the keys live, or the password.
pub fn settings(
    ui: &mut Ui,
    form: &mut Form,
    keystore: &mut Keystore,
    identity: &rusttalk_core::identity::DeviceIdentity,
) {
    let current = keystore.kind();
    let mut pick = form.pick.or(current).unwrap_or(Kind::File);
    let choices = [
        (
            Kind::Keychain,
            if cfg!(target_os = "android") {
                t!("keys.android_keystore")
            } else {
                t!("keys.keychain")
            },
            t!("keys.keychain_help"),
        ),
        (Kind::File, t!("keys.file"), t!("keys.file_help")),
        (
            Kind::Password,
            t!("keys.password_file"),
            t!("keys.password_file_help"),
        ),
    ];
    // Phones never offer the protected file.
    let choices: Vec<_> = choices
        .into_iter()
        .filter(|(kind, ..)| RULES.allows(*kind))
        .collect();
    if let Some(job) = form.busy.take_if(|job| job.done()) {
        let result = job
            .join()
            .and_then(|sealer| keystore.switch(identity, pick, Some(sealer)));
        finish(ui.ctx(), form, pick, result);
    } else if form.busy.is_some() {
        ui.ctx().request_repaint_after(POLL);
    }
    let busy = form.busy.is_some();
    ui::page_section(ui, t!("keys.section"));
    ui.add_enabled_ui(!busy, |ui| {
        let changed = ui::segmented(ui, &mut pick, &choices);
        if changed {
            *form = Form {
                pick: Some(pick),
                ..Form::default()
            };
            forget(ui.ctx());
        }
        ui.add_space(ui::SPACE_S);
        let needs_password = pick == Kind::Password;
        let entered = needs_password && form.password.show(ui, FIELD_WIDTH);
        let label = match (current == Some(pick), needs_password) {
            (true, false) => None,
            (true, true) => Some(t!("keys.change_password")),
            (false, _) => Some(t!("keys.move")),
        };
        let Some(label) = label else { return };
        ui.add_space(ui::SPACE_XS);
        let pressed = ui
            .horizontal(|ui| {
                let pressed = ui::primary(ui, label).clicked();
                if busy {
                    ui.spinner();
                }
                pressed
            })
            .inner;
        if !(pressed || entered) {
            return;
        }
        if !needs_password {
            let result = keystore.switch(identity, pick, None);
            finish(ui.ctx(), form, pick, result);
            return;
        }
        match form.password.check() {
            Ok(password) => {
                form.error = None;
                form.busy = Some(keystore.new_password(password));
            }
            Err(problem) => form.error = Some(problem.into()),
        }
    });
    error_text(ui, form.error.as_deref());
    if form.done {
        ui.label(ui::caption(t!("keys.moved")));
    }
}

fn finish(ctx: &Context, form: &mut Form, pick: Kind, result: anyhow::Result<()>) {
    *form = match result {
        Ok(()) => {
            forget(ctx);
            Form {
                done: true,
                ..Form::default()
            }
        }
        Err(error) => {
            tracing::error!(error = %format_args!("{error:#}"), "switching key storage failed");
            Form {
                pick: Some(pick),
                error: Some(if pick == Kind::Keychain {
                    t!("keys.no_keychain_here").to_owned()
                } else {
                    t!("keys.move_failed").to_owned()
                }),
                ..std::mem::take(form)
            }
        }
    };
}

/// The unlock screen's message for a failed attempt.
pub fn unlock_error(error: &anyhow::Error) -> String {
    if error.is::<WrongPassword>() {
        t!("keys.wrong_password").into()
    } else {
        tracing::error!(error = %format_args!("{error:#}"), "unlocking the key file failed");
        t!("keys.unlock_failed").into()
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::Vec2;
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    fn kept(ctx: &Context) -> usize {
        FIELDS
            .iter()
            .filter(|name| TextEditState::load(ctx, field_id(name)).is_some())
            .count()
    }

    #[test]
    fn typed_passwords_leave_no_state_behind() {
        let mut harness = Harness::builder()
            .with_size(Vec2::new(500.0, 500.0))
            .build_ui_state(
                |ui, state: &mut Option<(Zeroizing<String>, NewPassword)>| {
                    if let Some((password, new)) = state {
                        unlock(ui, password, None, 0, false);
                        new.show(ui, FIELD_WIDTH);
                    }
                },
                None,
            );
        ui::install(&harness.ctx);
        *harness.state_mut() = Some(Default::default());
        harness.run();
        for (index, field) in harness
            .query_all_by_label(t!("keys.password"))
            .enumerate()
            .collect::<Vec<_>>()
        {
            field.type_text(&format!("secret{index}"));
        }
        harness
            .get_by_label(t!("keys.repeat_password"))
            .type_text("secret1");
        harness.run();
        assert!(kept(&harness.ctx) >= 2);

        // Unlocking, cancelling or finishing forgets them.
        forget(&harness.ctx);
        assert_eq!(kept(&harness.ctx), 0);

        // So does leaving the settings page with a half-typed password.
        let mut form = Form::default();
        form.password.password.push_str("typed");
        TextEditState::default().store(&harness.ctx, field_id("new"));
        leave(&harness.ctx, &mut form);
        assert!(form.password.password.is_empty());
        assert_eq!(kept(&harness.ctx), 0);
    }
}
