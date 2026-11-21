use std::{cell::Cell, path::PathBuf};

use eframe::egui::{
    self, CentralPanel, CollapsingHeader, Grid, Key, Response, ScrollArea, Ui, Vec2,
};
use rusttalk_core::identity::{DeviceIdentity, IdentityError, WordConfirmation};
use zeroize::Zeroizing;

use crate::{
    home::Home,
    keystore::Keystore,
    ui::{self, error_text, primary_large as primary, secondary_large as secondary},
};

const DEVICE_NAME: &str = "Desktop";
const CARD_WIDTH: f32 = 560.0;

pub struct App<'a> {
    keystore: Keystore,
    screen: Screen,
    ctx: egui::Context,
    /// The profile folder, for saved servers.
    data: PathBuf,
    updates: crate::update::Updater,
    window_started: &'a Cell<bool>,
}

enum Screen {
    Problem(String),
    Welcome,
    Words {
        words: Zeroizing<String>,
        identity: DeviceIdentity,
    },
    Confirm {
        words: Zeroizing<String>,
        identity: DeviceIdentity,
        check: WordConfirmation,
        answers: [Zeroizing<String>; 3],
        error: Option<String>,
    },
    Restore {
        words: Zeroizing<String>,
        error: Option<String>,
    },
    Home(Box<Home>),
}

pub enum Action {
    Create,
    Restore,
    StartOver,
    Continue,
    SignOut,
}

impl<'a> App<'a> {
    pub fn new(
        cc: &eframe::CreationContext,
        keystore: Keystore,
        identity: Option<DeviceIdentity>,
        data: PathBuf,
        window_started: &'a Cell<bool>,
    ) -> Self {
        ui::install(&cc.egui_ctx);
        #[cfg(target_os = "linux")]
        crate::wayland_drops::install(cc);
        let updates = crate::update::Updater::new(&data);
        let mut app = Self {
            keystore,
            screen: Screen::Welcome,
            ctx: cc.egui_ctx.clone(),
            data,
            updates,
            window_started,
        };
        if let Some(identity) = identity {
            app.screen = app.home(identity);
        }
        app
    }

    fn home(&self, identity: DeviceIdentity) -> Screen {
        match Home::new(identity, &self.ctx, self.data.clone()) {
            Ok(home) => Screen::Home(Box::new(home)),
            Err(error) => {
                tracing::error!(error = %format_args!("{error:#}"), "starting the network failed");
                Screen::Problem(format!("{error:#}"))
            }
        }
    }

    fn apply(&mut self, action: Action) {
        let keystore = &self.keystore;
        let screen = std::mem::replace(&mut self.screen, Screen::Welcome);
        self.screen = match (action, screen) {
            (Action::StartOver, _) => Screen::Welcome,
            (Action::Create, _) => match DeviceIdentity::create(DEVICE_NAME) {
                Ok((words, identity)) => Screen::Words { words, identity },
                Err(error) => problem("create account", &error),
            },
            (Action::Restore, _) => Screen::Restore {
                words: Zeroizing::default(),
                error: None,
            },
            (Action::Continue, Screen::Words { words, identity }) => {
                match WordConfirmation::random() {
                    Ok(check) => Screen::Confirm {
                        words,
                        identity,
                        check,
                        answers: Default::default(),
                        error: None,
                    },
                    Err(error) => problem("pick words to confirm", &error),
                }
            }
            (
                Action::Continue,
                Screen::Confirm {
                    words,
                    identity,
                    check,
                    answers,
                    ..
                },
            ) => {
                let error = if check.matches(&words, answers.each_ref().map(|a| a.as_str())) {
                    save(keystore, &identity)
                } else {
                    Some(t!("app.words_mismatch").into())
                };
                match error {
                    None => self.home(identity),
                    error => Screen::Confirm {
                        words,
                        identity,
                        check,
                        answers,
                        error,
                    },
                }
            }
            (Action::Continue, Screen::Restore { words, .. }) => {
                let error = match DeviceIdentity::add_device(&words, DEVICE_NAME) {
                    Ok(identity) => match save(keystore, &identity) {
                        None => {
                            tracing::info!("added this device to an existing account");
                            self.screen = self.home(identity);
                            if let Screen::Home(home) = &mut self.screen {
                                home.restored = true;
                            }
                            return;
                        }
                        error => error,
                    },
                    Err(IdentityError::InvalidWords) => {
                        tracing::info!("recovery words rejected");
                        Some(t!("app.words_invalid").into())
                    }
                    Err(error) => {
                        tracing::error!(%error, "add device failed");
                        Some(t!("app.add_device_failed", error = error))
                    }
                };
                Screen::Restore { words, error }
            }
            (Action::SignOut, Screen::Home(mut home)) => match keystore.delete() {
                Ok(()) => {
                    // Stop the network first, so nothing writes these files again.
                    drop(home);
                    forget_profile(&self.data);
                    crate::files::forget_previews(&self.ctx);
                    tracing::info!("signed out of this device");
                    Screen::Welcome
                }
                Err(error) => {
                    tracing::error!(error = %format_args!("{error:#}"), "sign out failed");
                    home.confirm_sign_out = false;
                    home.error = Some(t!("app.sign_out_failed").into());
                    Screen::Home(home)
                }
            },
            (_, screen) => screen,
        };
    }
}

/// Remove what this profile remembers about the person who signed out: joined servers
/// and their sync state, server pins, recent emoji, where files were saved or opened,
/// read marks, notification choices and pokes.
/// Device settings stay.
fn forget_profile(data: &std::path::Path) {
    crate::files::clear_opened();
    for name in [
        "sync.json",
        "servers.json",
        "saved.json",
        "notify.json",
        "pokes.json",
        crate::emoji::picker::RECENT_FILE,
        crate::emoji::picker::FAVOURITES_FILE,
    ] {
        match std::fs::remove_file(data.join(name)) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                tracing::error!(%error, file = name, "removing profile data at sign out failed");
            }
            _ => {}
        }
    }
}

/// Returns the message to show when saving failed.
fn save(keystore: &Keystore, identity: &DeviceIdentity) -> Option<String> {
    let error = keystore.save(identity).err()?;
    tracing::error!(error = %format_args!("{error:#}"), "saving identity failed");
    Some(t!("app.save_failed").into())
}

fn problem(what: &str, error: &IdentityError) -> Screen {
    tracing::error!(%error, "{what} failed");
    Screen::Problem(format!("{what}: {error}"))
}

impl eframe::App for App<'_> {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        crate::paste::keys(ctx, raw_input);
        #[cfg(target_os = "linux")]
        crate::wayland_drops::input(raw_input);
    }

    fn logic(&mut self, _ctx: &egui::Context, frame: &mut eframe::Frame) {
        // eframe reports frame time after painting and showing the first frame.
        if frame.info().cpu_usage.is_some() {
            self.window_started.set(true);
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.updates.poll(ui.ctx());
        let action = match &mut self.screen {
            Screen::Problem(details) => card(ui, |ui| {
                ui.heading(t!("app.problem_title"));
                ui.label(t!("app.problem_help"));
                CollapsingHeader::new(t!("common.details"))
                    .show(ui, |ui| ui.label(details.as_str()));
                None
            }),
            Screen::Welcome => card(ui, welcome),
            Screen::Words { words, .. } => card(ui, |ui| show_words(ui, words)),
            Screen::Confirm {
                check,
                answers,
                error,
                ..
            } => card(ui, |ui| confirm(ui, check, answers, error.as_deref())),
            Screen::Restore { words, error } => card(ui, |ui| restore(ui, words, error.as_deref())),
            Screen::Home(home) => crate::home::show(ui, home, &mut self.updates),
        };
        if let Some(action) = action {
            self.apply(action);
        }
        crate::paste::text(ui.ctx());
    }
}

/// A centered column for the sign-up screens.
fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> Option<Action>) -> Option<Action> {
    CentralPanel::default()
        .show(ui, |ui| {
            ScrollArea::vertical()
                .show(ui, |ui| {
                    let width = CARD_WIDTH.min(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - width) / 2.0);
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            ui.add_space(48.0);
                            add(ui)
                        })
                        .inner
                    })
                    .inner
                })
                .inner
        })
        .inner
}

pub fn submitted(ui: &Ui, field: &Response) -> bool {
    field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter))
}

fn welcome(ui: &mut Ui) -> Option<Action> {
    ui.heading(t!("app.welcome"));
    ui.label(t!("app.welcome_text"));
    ui.add_space(24.0);
    if primary(ui, t!("app.create_account")).clicked() {
        return Some(Action::Create);
    }
    secondary(ui, t!("app.have_account"))
        .clicked()
        .then_some(Action::Restore)
}

fn show_words(ui: &mut Ui, words: &str) -> Option<Action> {
    ui.heading(t!("app.words_title"));
    ui.label(t!("app.words_text"));
    ui.add_space(12.0);
    Grid::new("recovery-words")
        .num_columns(4)
        .spacing(Vec2::new(10.0, 10.0))
        .show(ui, |ui| {
            for (index, word) in words.split_whitespace().enumerate() {
                ui::card_frame(ui).show(ui, |ui| {
                    ui.set_width(110.0);
                    ui.label(ui::caption(format!("{}", index + 1)));
                    ui.label(ui::bold(word).size(16.0));
                });
                if index % 4 == 3 {
                    ui.end_row();
                }
            }
        });
    ui.add_space(16.0);
    primary(ui, t!("app.wrote_them"))
        .clicked()
        .then_some(Action::Continue)
}

fn confirm(
    ui: &mut Ui,
    check: &WordConfirmation,
    answers: &mut [Zeroizing<String>; 3],
    error: Option<&str>,
) -> Option<Action> {
    ui.heading(t!("app.check_title"));
    ui.label(t!("app.check_text"));
    ui.add_space(12.0);
    let mut done = false;
    for (slot, (answer, index)) in answers.iter_mut().zip(check.indices()).enumerate() {
        let label = ui.label(t!("app.word_number", number = index + 1));
        let field = ui
            .add(ui::field(&mut **answer).desired_width(260.0))
            .labelled_by(label.id);
        if slot == 0 && ui.memory(|memory| memory.focused().is_none()) {
            field.request_focus();
        }
        done |= submitted(ui, &field);
    }
    error_text(ui, error);
    ui.add_space(12.0);
    if primary(ui, t!("app.confirm")).clicked() || done {
        return Some(Action::Continue);
    }
    secondary(ui, t!("app.start_over"))
        .clicked()
        .then_some(Action::StartOver)
}

fn restore(ui: &mut Ui, words: &mut Zeroizing<String>, error: Option<&str>) -> Option<Action> {
    ui.heading(t!("app.restore_title"));
    ui.label(t!("app.restore_text"));
    ui.add_space(12.0);
    let label = ui.label(t!("app.recovery_words"));
    let field = ui
        .add(
            ui::text_area(&mut **words)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        )
        .labelled_by(label.id);
    if ui.memory(|memory| memory.focused().is_none()) {
        field.request_focus();
    }
    error_text(ui, error);
    ui.add_space(12.0);
    if primary(ui, t!("app.sign_in")).clicked() {
        return Some(Action::Continue);
    }
    secondary(ui, t!("common.back"))
        .clicked()
        .then_some(Action::StartOver)
}
