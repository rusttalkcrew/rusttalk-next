//! The key storage screens: the first-start choice without a keychain, the password
//! unlock, the missing-keychain help, and the section in Settings > Privacy. The
//! start screens use the big buttons and take Enter, for the Steam Deck's on-screen
//! keyboard and gamepad.

use eframe::egui::Ui;
use zeroize::Zeroizing;

use crate::{
    app::submitted,
    keystore::{Keystore, Kind, WrongPassword},
    ui::{self, error_text, primary_large, secondary_large},
};

/// Wrong passwords before the hint about the recovery words.
pub const HINT_AFTER: u32 = 3;

/// A password typed twice.
#[derive(Default)]
pub struct NewPassword {
    pub password: Zeroizing<String>,
    pub repeat: Zeroizing<String>,
}

impl NewPassword {
    /// The fields; true when Enter was pressed in the last one.
    fn show(&mut self, ui: &mut Ui, width: f32) -> bool {
        let label = ui.label(t!("keys.password"));
        ui.add(
            ui::field(&mut *self.password)
                .password()
                .desired_width(width),
        )
        .labelled_by(label.id);
        let label = ui.label(t!("keys.repeat_password"));
        let repeat = ui
            .add(ui::field(&mut *self.repeat).password().desired_width(width))
            .labelled_by(label.id);
        submitted(ui, &repeat)
    }

    /// The password, or what's wrong with it.
    fn check(&self) -> Result<&str, &'static str> {
        if self.password.is_empty() {
            Err(t!("keys.password_empty"))
        } else if *self.password != *self.repeat {
            Err(t!("keys.password_mismatch"))
        } else {
            Ok(&self.password)
        }
    }
}

pub enum Picked {
    File,
    Password,
}

/// First start without a keychain. Returns the choice once it's complete.
pub fn choose(
    ui: &mut Ui,
    password: &mut Option<NewPassword>,
    error: &mut Option<String>,
) -> Option<Picked> {
    ui.heading(t!("keys.choose_title"));
    ui.label(t!("keys.no_keychain"));
    ui.add_space(ui::SPACE_L);
    if primary_large(ui, t!("keys.file")).clicked() {
        return Some(Picked::File);
    }
    ui.label(ui::caption(t!("keys.file_help")));
    ui.add_space(ui::SPACE_S);
    if secondary_large(ui, t!("keys.password_file")).clicked() && password.is_none() {
        *password = Some(NewPassword::default());
    }
    ui.label(ui::caption(t!("keys.password_file_help")));
    let fields = password.as_mut()?;
    ui.add_space(ui::SPACE_L);
    let entered = fields.show(ui, 320.0);
    error_text(ui, error.as_deref());
    ui.add_space(ui::SPACE_S);
    if !(primary_large(ui, t!("keys.continue")).clicked() || entered) {
        return None;
    }
    match fields.check() {
        Ok(_) => Some(Picked::Password),
        Err(problem) => {
            *error = Some(problem.into());
            None
        }
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
) -> Option<Unlock> {
    ui.heading(t!("keys.unlock_title"));
    ui.label(t!("keys.unlock_text"));
    ui.add_space(ui::SPACE_M);
    let label = ui.label(t!("keys.password"));
    let field = ui
        .add(ui::field(&mut **password).password().desired_width(320.0))
        .labelled_by(label.id);
    if ui.memory(|memory| memory.focused().is_none()) {
        field.request_focus();
    }
    let entered = submitted(ui, &field);
    error_text(ui, error);
    ui.add_space(ui::SPACE_S);
    if primary_large(ui, t!("keys.unlock")).clicked() || entered {
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
}

/// The keys are in a keychain this session can't reach. True for "use recovery words".
pub fn keychain_gone(ui: &mut Ui) -> bool {
    ui.heading(t!("keys.gone_title"));
    ui.label(t!("keys.gone_text"));
    ui.add_space(ui::SPACE_L);
    secondary_large(ui, t!("keys.use_words")).clicked()
}

/// What the Key storage section in Settings is doing.
#[derive(Default)]
pub struct Form {
    pick: Option<Kind>,
    password: NewPassword,
    error: Option<String>,
    done: bool,
}

/// Settings > Privacy > Key storage: switch where the keys live, or the password.
pub fn settings(
    ui: &mut Ui,
    form: &mut Form,
    keystore: &mut Keystore,
    identity: &rusttalk_core::identity::DeviceIdentity,
) {
    let current = keystore.kind();
    ui::page_section(ui, t!("keys.section"));
    let mut pick = form.pick.or(current).unwrap_or(Kind::File);
    let changed = ui::segmented(
        ui,
        &mut pick,
        &[
            (
                Kind::Keychain,
                t!("keys.keychain"),
                t!("keys.keychain_help"),
            ),
            (Kind::File, t!("keys.file"), t!("keys.file_help")),
            (
                Kind::Password,
                t!("keys.password_file"),
                t!("keys.password_file_help"),
            ),
        ],
    );
    if changed {
        *form = Form {
            pick: Some(pick),
            ..Form::default()
        };
    }
    ui.add_space(ui::SPACE_S);
    let needs_password = pick == Kind::Password;
    let entered = needs_password && form.password.show(ui, 320.0);
    let label = match (current == Some(pick), needs_password) {
        (true, false) => None,
        (true, true) => Some(t!("keys.change_password")),
        (false, _) => Some(t!("keys.move")),
    };
    if let Some(label) = label {
        ui.add_space(ui::SPACE_XS);
        if ui::primary(ui, label).clicked() || entered {
            let password = if needs_password {
                form.password.check()
            } else {
                Ok("")
            };
            let result = password.map_err(str::to_owned).and_then(|password| {
                keystore.switch(identity, pick, password).map_err(|error| {
                    tracing::error!(error = %format_args!("{error:#}"), "switching key storage failed");
                    if pick == Kind::Keychain {
                        t!("keys.no_keychain_here").to_owned()
                    } else {
                        t!("keys.move_failed").to_owned()
                    }
                })
            });
            *form = match result {
                Ok(()) => Form {
                    done: true,
                    ..Form::default()
                },
                Err(error) => Form {
                    pick: Some(pick),
                    error: Some(error),
                    ..std::mem::take(form)
                },
            };
        }
    }
    error_text(ui, form.error.as_deref());
    if form.done {
        ui.label(ui::caption(t!("keys.moved")));
    }
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
