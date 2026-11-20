use std::path::PathBuf;

use anyhow::Result;
use eframe::egui::{
    self, Button, CentralPanel, CollapsingHeader, Frame, Grid, Key, Response, RichText, ScrollArea,
    TextEdit, TextStyle, Ui, Vec2,
};
use rusttalk_core::identity::{DeviceIdentity, IdentityError, WordConfirmation};
use zeroize::Zeroizing;

use crate::{home::Home, keystore::Keystore};

const DEVICE_NAME: &str = "Desktop";
const CARD_WIDTH: f32 = 560.0;

pub struct App {
    /// `None` only when startup failed and the problem screen is showing.
    keystore: Option<Keystore>,
    screen: Screen,
    ctx: egui::Context,
    /// The profile folder, for saved servers.
    data: PathBuf,
    updates: crate::update::Updater,
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

impl App {
    pub fn new(
        cc: &eframe::CreationContext,
        opened: Result<(Keystore, Option<DeviceIdentity>)>,
        data: PathBuf,
    ) -> Self {
        cc.egui_ctx.all_styles_mut(|style| {
            for (text_style, font) in &mut style.text_styles {
                font.size = match text_style {
                    TextStyle::Heading => 24.0,
                    TextStyle::Small => 12.0,
                    _ => 15.0,
                };
            }
            style.spacing.item_spacing = Vec2::new(8.0, 8.0);
            style.spacing.button_padding = Vec2::new(10.0, 6.0);
        });
        // Brighter body text, so chat reads easily.
        cc.egui_ctx.style_mut_of(egui::Theme::Dark, |style| {
            style.visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_gray(215);
        });
        cc.egui_ctx.style_mut_of(egui::Theme::Light, |style| {
            style.visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_gray(35);
        });
        let updates = crate::update::Updater::new(&data, &cc.egui_ctx);
        let mut app = Self {
            keystore: None,
            screen: Screen::Welcome,
            ctx: cc.egui_ctx.clone(),
            data,
            updates,
        };
        match opened {
            Ok((keystore, identity)) => {
                app.keystore = Some(keystore);
                if let Some(identity) = identity {
                    app.screen = app.home(identity);
                }
            }
            Err(error) => app.screen = Screen::Problem(format!("{error:#}")),
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
        let Some(keystore) = &self.keystore else {
            return;
        };
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
                    Some("Those words don't match your list. Check them and try again.".into())
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
                            return;
                        }
                        error => error,
                    },
                    Err(IdentityError::InvalidWords) => {
                        tracing::info!("recovery words rejected");
                        Some("Those words don't look right. Check the spelling and that there are 24.".into())
                    }
                    Err(error) => {
                        tracing::error!(%error, "add device failed");
                        Some(format!("Couldn't add this device: {error}"))
                    }
                };
                Screen::Restore { words, error }
            }
            (Action::SignOut, Screen::Home(mut home)) => match keystore.delete() {
                Ok(()) => {
                    // Stop the network first, so nothing writes these files again.
                    drop(home);
                    forget_profile(&self.data);
                    tracing::info!("signed out of this device");
                    Screen::Welcome
                }
                Err(error) => {
                    tracing::error!(error = %format_args!("{error:#}"), "sign out failed");
                    home.confirm_sign_out = false;
                    home.error = Some("Couldn't sign out. Try again.".into());
                    Screen::Home(home)
                }
            },
            (_, screen) => screen,
        };
    }
}

/// Remove what this profile remembers about the person who signed out: joined servers
/// and their sync state, server pins, where files were saved and files opened from chat.
/// Device settings stay.
fn forget_profile(data: &std::path::Path) {
    crate::files::clear_opened();
    for name in ["sync.json", "servers.json", "saved.json"] {
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
    Some("Couldn't save your account on this computer. Try again.".into())
}

fn problem(what: &str, error: &IdentityError) -> Screen {
    tracing::error!(%error, "{what} failed");
    Screen::Problem(format!("{what}: {error}"))
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.updates.poll(ui.ctx());
        let action = match &mut self.screen {
            Screen::Problem(details) => card(ui, |ui| {
                ui.heading("RustTalk couldn't start");
                ui.label("Close RustTalk and open it again. If this keeps happening, check that your system keyring is unlocked.");
                CollapsingHeader::new("Details").show(ui, |ui| ui.label(details.as_str()));
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

pub fn primary(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        Button::new(RichText::new(text).size(16.0))
            .fill(ui.visuals().selection.bg_fill)
            .min_size(Vec2::new(240.0, 40.0)),
    )
}

pub fn secondary(ui: &mut Ui, text: &str) -> Response {
    ui.add(Button::new(RichText::new(text).size(16.0)).min_size(Vec2::new(240.0, 40.0)))
}

pub fn error_text(ui: &mut Ui, error: Option<&str>) {
    if let Some(error) = error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
}

pub fn submitted(ui: &Ui, field: &Response) -> bool {
    field.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter))
}

fn welcome(ui: &mut Ui) -> Option<Action> {
    ui.heading("Welcome to RustTalk");
    ui.label("Talk and chat with your friends. Private and encrypted.");
    ui.add_space(24.0);
    if primary(ui, "Create account").clicked() {
        return Some(Action::Create);
    }
    secondary(ui, "I already have an account")
        .clicked()
        .then_some(Action::Restore)
}

fn show_words(ui: &mut Ui, words: &str) -> Option<Action> {
    ui.heading("Your recovery words");
    ui.label("Write these 24 words on paper, in this order, and keep them safe. They are the only way back into your account. You will see them only once.");
    ui.add_space(12.0);
    Grid::new("recovery-words")
        .num_columns(4)
        .spacing(Vec2::new(10.0, 10.0))
        .show(ui, |ui| {
            for (index, word) in words.split_whitespace().enumerate() {
                Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(110.0);
                    ui.label(RichText::new(format!("{}", index + 1)).weak());
                    ui.label(RichText::new(word).strong().size(16.0));
                });
                if index % 4 == 3 {
                    ui.end_row();
                }
            }
        });
    ui.add_space(16.0);
    primary(ui, "I wrote them down")
        .clicked()
        .then_some(Action::Continue)
}

fn confirm(
    ui: &mut Ui,
    check: &WordConfirmation,
    answers: &mut [Zeroizing<String>; 3],
    error: Option<&str>,
) -> Option<Action> {
    ui.heading("Check your words");
    ui.label("Type these words from your list.");
    ui.add_space(12.0);
    let mut done = false;
    for (slot, (answer, index)) in answers.iter_mut().zip(check.indices()).enumerate() {
        let label = ui.label(format!("Word {}", index + 1));
        let field = ui
            .add(TextEdit::singleline(&mut **answer).desired_width(260.0))
            .labelled_by(label.id);
        if slot == 0 && ui.memory(|memory| memory.focused().is_none()) {
            field.request_focus();
        }
        done |= submitted(ui, &field);
    }
    error_text(ui, error);
    ui.add_space(12.0);
    if primary(ui, "Confirm").clicked() || done {
        return Some(Action::Continue);
    }
    secondary(ui, "Start over")
        .clicked()
        .then_some(Action::StartOver)
}

fn restore(ui: &mut Ui, words: &mut Zeroizing<String>, error: Option<&str>) -> Option<Action> {
    ui.heading("Sign in with your recovery words");
    ui.label("Type your 24 recovery words in order, with a space between them. This computer becomes a new device on your account.");
    ui.add_space(12.0);
    let label = ui.label("Recovery words");
    let field = ui
        .add(
            TextEdit::multiline(&mut **words)
                .desired_rows(4)
                .desired_width(f32::INFINITY),
        )
        .labelled_by(label.id);
    if ui.memory(|memory| memory.focused().is_none()) {
        field.request_focus();
    }
    error_text(ui, error);
    ui.add_space(12.0);
    if primary(ui, "Sign in").clicked() {
        return Some(Action::Continue);
    }
    secondary(ui, "Back").clicked().then_some(Action::StartOver)
}
