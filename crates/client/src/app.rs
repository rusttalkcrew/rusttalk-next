use std::{cell::Cell, path::PathBuf};

use eframe::egui::{
    self, Align, CollapsingHeader, Grid, Id, Key, Layout, Margin, Response, Ui, Vec2,
};
use rusttalk_core::identity::{DeviceIdentity, IdentityError, WordConfirmation};
use zeroize::Zeroizing;

use crate::{
    home::Home,
    key_storage::{self, NewPassword, Picked, Unlock},
    keystore::{Chosen, Job, Keystore, Kind, Sealer, Start, Unlocked},
    ui::{self, error_text, primary_large as primary, secondary_large as secondary},
    words,
};

const DEVICE_NAME: &str = "Desktop";
/// The recovery words in a list.
const WORD_COUNT: usize = 24;
/// How long "Copied" shows after copying the words.
const COPIED_SECONDS: f64 = 3.0;

pub struct App<'a> {
    keystore: Keystore,
    screen: Screen,
    ctx: egui::Context,
    /// The profile folder, for saved servers.
    data: PathBuf,
    updates: crate::update::Updater,
    window_started: &'a Cell<bool>,
    /// The key storage choice came first, so Back on the welcome screen returns to it.
    chose_storage: bool,
    /// The recovery words were asked for from here: Back returns to it.
    recover: Option<Recover>,
}

/// Where "use recovery words" was pressed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Recover {
    Unlock,
    Missing { keychain: bool },
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
    /// No keychain on the first start; `restore` goes on to the recovery words.
    Storage {
        password: Option<NewPassword>,
        error: Option<String>,
        restore: bool,
        /// The key for the chosen password, being derived.
        busy: Option<Job<Sealer>>,
    },
    Unlock {
        password: Zeroizing<String>,
        error: Option<String>,
        tries: u32,
        busy: Option<Job<Unlocked>>,
    },
    /// This profile's keys aren't found; `keychain` when they're in an unreachable one.
    Missing {
        keychain: bool,
    },
    Home(Box<Home>),
}

pub enum Action {
    Create,
    Restore,
    /// The step before this one.
    Back,
    Continue,
    SignOut,
    Choose(Picked),
    Unlock,
    UseWords,
    /// Password work finished.
    Sealed(anyhow::Result<Sealer>),
    Unlocked(anyhow::Result<Unlocked>),
}

impl<'a> App<'a> {
    pub fn new(
        cc: &eframe::CreationContext,
        keystore: Keystore,
        start: Start,
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
            chose_storage: matches!(start, Start::Choose),
            recover: None,
        };
        app.screen = match start {
            Start::Ready(Some(identity)) => app.home(*identity),
            Start::Ready(None) => Screen::Welcome,
            Start::Choose => Screen::Storage {
                password: None,
                error: None,
                restore: false,
                busy: None,
            },
            Start::Unlock => Screen::Unlock {
                password: key_storage::secret_input(),
                error: None,
                tries: 0,
                busy: None,
            },
            Start::Missing { keychain } => Screen::Missing { keychain },
        };
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

    /// After the first-start choice: on to the account screens.
    fn chosen(&mut self, kind: Kind, sealer: Option<Sealer>, restore: bool) -> Screen {
        match self.keystore.choose(kind, sealer) {
            Ok(Chosen::Opened(identity)) => {
                key_storage::forget(&self.ctx);
                self.home(*identity)
            }
            Ok(Chosen::New) => {
                key_storage::forget(&self.ctx);
                if restore {
                    Screen::Restore {
                        words: key_storage::secret_input(),
                        error: None,
                    }
                } else {
                    Screen::Welcome
                }
            }
            Err(error) => {
                tracing::error!(error = %format_args!("{error:#}"), "choosing key storage failed");
                Screen::Storage {
                    password: None,
                    error: Some(t!("keys.choose_failed").into()),
                    restore,
                    busy: None,
                }
            }
        }
    }

    /// Pick up finished password work.
    fn poll(&mut self) {
        let action = match &mut self.screen {
            Screen::Storage { busy, .. } => busy
                .take_if(|job| job.done())
                .map(|job| Action::Sealed(job.join())),
            Screen::Unlock { busy, .. } => busy
                .take_if(|job| job.done())
                .map(|job| Action::Unlocked(job.join())),
            _ => return,
        };
        match action {
            Some(action) => self.apply(action),
            None => {
                if matches!(
                    self.screen,
                    Screen::Storage { busy: Some(_), .. } | Screen::Unlock { busy: Some(_), .. }
                ) {
                    self.ctx.request_repaint_after(key_storage::POLL);
                }
            }
        }
    }

    fn apply(&mut self, action: Action) {
        // Whatever comes next, egui keeps nothing of the recovery-word fields.
        forget_words(&self.ctx);
        let screen = std::mem::replace(&mut self.screen, Screen::Welcome);
        self.screen = match (action, screen) {
            (Action::Back, screen) => {
                // Leaving a password screen: forget what egui kept of the fields.
                key_storage::forget(&self.ctx);
                back(screen, self.chose_storage, self.recover)
            }
            (Action::Create, _) => match DeviceIdentity::create(DEVICE_NAME) {
                Ok((words, identity)) => Screen::Words { words, identity },
                Err(error) => problem("create account", &error),
            },
            (Action::Restore, _) => Screen::Restore {
                words: key_storage::secret_input(),
                error: None,
            },
            (Action::Continue, Screen::Words { words, identity }) => {
                match WordConfirmation::random() {
                    Ok(check) => Screen::Confirm {
                        words,
                        identity,
                        check,
                        answers: std::array::from_fn(|_| key_storage::secret_input()),
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
                    save(&mut self.keystore, &identity)
                } else {
                    Some(t!("app.words_mismatch").into())
                };
                match error {
                    None => {
                        let mut screen = self.home(identity);
                        if let Screen::Home(home) = &mut screen {
                            home.new_account = true;
                        }
                        screen
                    }
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
                    Ok(identity) => match save(&mut self.keystore, &identity) {
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
            (Action::UseWords, screen) => {
                self.recover = match screen {
                    Screen::Missing { keychain } => Some(Recover::Missing { keychain }),
                    _ => Some(Recover::Unlock),
                };
                key_storage::forget(&self.ctx);
                Screen::Storage {
                    password: None,
                    error: None,
                    restore: true,
                    busy: None,
                }
            }
            (Action::Choose(Picked::File), Screen::Storage { restore, .. }) => {
                self.chosen(Kind::File, None, restore)
            }
            (
                Action::Choose(Picked::Password(typed)),
                Screen::Storage {
                    password,
                    restore,
                    busy: None,
                    ..
                },
            ) => Screen::Storage {
                password,
                error: None,
                restore,
                busy: Some(self.keystore.new_password(typed)),
            },
            (
                Action::Sealed(sealer),
                Screen::Storage {
                    password, restore, ..
                },
            ) => match sealer {
                Ok(sealer) => self.chosen(Kind::Password, Some(sealer), restore),
                Err(error) => {
                    tracing::error!(error = %format_args!("{error:#}"), "choosing a password failed");
                    Screen::Storage {
                        password,
                        error: Some(t!("keys.choose_failed").into()),
                        restore,
                        busy: None,
                    }
                }
            },
            (
                Action::Unlock,
                Screen::Unlock {
                    password,
                    tries,
                    busy: None,
                    ..
                },
            ) => match self.keystore.start_unlock(password.clone()) {
                Ok(job) => Screen::Unlock {
                    password,
                    error: None,
                    tries,
                    busy: Some(job),
                },
                Err(error) => Screen::Unlock {
                    password,
                    error: Some(key_storage::unlock_error(&error)),
                    tries,
                    busy: None,
                },
            },
            (Action::Unlocked(unlocked), Screen::Unlock { tries, .. }) => {
                match unlocked.and_then(|unlocked| self.keystore.finish_unlock(unlocked)) {
                    Ok(identity) => {
                        key_storage::forget(&self.ctx);
                        self.home(identity)
                    }
                    Err(error) => {
                        key_storage::forget(&self.ctx);
                        Screen::Unlock {
                            password: key_storage::secret_input(),
                            error: Some(key_storage::unlock_error(&error)),
                            tries: tries + 1,
                            busy: None,
                        }
                    }
                }
            }
            (Action::SignOut, Screen::Home(mut home)) => match self.keystore.delete() {
                Ok(()) => {
                    // Stop the network first, so nothing writes these files again.
                    home.stop_for_sign_out();
                    let settings = home.voice.audio.settings();
                    drop(home);
                    crate::files::clear_opened();
                    forget_profile(&self.data, settings);
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

/// Keep device setup in this shared config directory, including both process locks.
fn forget_profile(data: &std::path::Path, settings: crate::audio::Settings) {
    use std::fs;
    let remove = |name, result: std::io::Result<()>| {
        if let Err(error) = result
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!(%error, file = name, "removing profile data at sign out failed");
        }
    };
    for name in [
        "sync.json",
        "servers.json",
        "saved.json",
        "notify.json",
        "pokes.json",
        "dm_deleted.json",
        crate::emoji::picker::RECENT_FILE,
        crate::emoji::picker::FAVOURITES_FILE,
        "rusttalk.log.1",
        "rusttalk.log",
    ] {
        remove(name, fs::remove_file(data.join(name)));
    }
    remove("revocations", fs::remove_dir_all(data.join("revocations")));
    remove("dm_pins", fs::remove_dir_all(data.join("dm_pins")));
    crate::previews::forget(data);

    // Only carry hardware and window preferences into the next account.
    let device = crate::audio::Settings {
        input: settings.input,
        output: settings.output,
        camera: settings.camera,
        call_sounds: settings.call_sounds,
        ai_noise: settings.ai_noise,
        noise_strength: settings.noise_strength,
        mic_hint: settings.mic_hint,
        channels_width: settings.channels_width,
        members_width: settings.members_width,
        members_hidden: settings.members_hidden,
        call_side_by_side: settings.call_side_by_side,
        call_split: settings.call_split,
        call_chat_hidden: settings.call_chat_hidden,
        call_stage_hidden: settings.call_stage_hidden,
        call_video_only: settings.call_video_only,
        call_show_fps: settings.call_show_fps,
        call_nerd_stats: settings.call_nerd_stats,
        screen_height: settings.screen_height,
        screen_fps: settings.screen_fps,
        screen_cursor: settings.screen_cursor,
        whats_new_seen: settings.whats_new_seen,
        ..Default::default()
    };
    let path = data.join("voice.json");
    if device == Default::default() {
        remove("voice.json", fs::remove_file(path));
    } else if let Err(error) = rusttalk_core::client::save_json(data, &path, &device) {
        tracing::error!(%error, "saving device preferences at sign out failed");
        remove("voice.json", fs::remove_file(path));
    }
}

/// Returns the message to show when saving failed.
fn save(keystore: &mut Keystore, identity: &DeviceIdentity) -> Option<String> {
    let error = keystore.save(identity).err()?;
    tracing::error!(error = %format_args!("{error:#}"), "saving identity failed");
    Some(t!("app.save_failed").into())
}

/// The step before `screen`. Nothing is saved before the word check or the
/// recovery words succeed, so going back leaves no half-made account: the words of
/// a new account stay the same back to their own screen, and are dropped (wiped)
/// before it.
fn back(screen: Screen, chose_storage: bool, recover: Option<Recover>) -> Screen {
    let storage = |restore| Screen::Storage {
        password: None,
        error: None,
        restore,
        busy: None,
    };
    match screen {
        Screen::Welcome if chose_storage => storage(false),
        Screen::Words { .. } => Screen::Welcome,
        Screen::Confirm {
            words, identity, ..
        } => Screen::Words { words, identity },
        Screen::Restore { .. } if recover.is_some() => storage(true),
        Screen::Restore { .. } => Screen::Welcome,
        Screen::Storage {
            restore: true,
            busy: None,
            ..
        } => match recover {
            Some(Recover::Missing { keychain }) => Screen::Missing { keychain },
            _ => Screen::Unlock {
                password: key_storage::secret_input(),
                error: None,
                tries: 0,
                busy: None,
            },
        },
        screen => screen,
    }
}

fn problem(what: &str, error: &IdentityError) -> Screen {
    tracing::error!(%error, "{what} failed");
    Screen::Problem(format!("{what}: {error}"))
}

impl eframe::App for App<'_> {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        #[cfg(target_os = "ios")]
        crate::ios::input(ctx, raw_input);
        crate::paste::keys(ctx, raw_input);
        #[cfg(target_os = "android")]
        crate::android::input(ctx, raw_input);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        crate::phone_clipboard::input(ctx, raw_input);
        #[cfg(target_os = "linux")]
        crate::wayland_drops::input(raw_input);
    }

    fn logic(&mut self, _ctx: &egui::Context, frame: &mut eframe::Frame) {
        // eframe reports frame time after painting and showing the first frame.
        if frame.info().cpu_usage.is_some() {
            self.window_started.set(true);
        }
        // Here, not in `ui`: a hidden window gets no `ui` pass on Wayland.
        if let Screen::Home(home) = &mut self.screen {
            crate::hotkeys::frame(home);
        }
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        #[cfg(target_os = "ios")]
        crate::ios::accessibility(ui.ctx());
        #[cfg(target_os = "android")]
        crate::android::frame(ui, |ui| self.screens(ui));
        #[cfg(target_os = "ios")]
        crate::ios::frame(ui, frame.winit_window().map(|window| &**window), |ui| {
            self.screens(ui)
        });
        #[cfg(not(target_os = "ios"))]
        let _ = frame;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        self.screens(ui);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        crate::phone_clipboard::frame_end(ui.ctx());
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        crate::words::frame(ui.ctx());
    }
}

impl App<'_> {
    fn screens(&mut self, ui: &mut Ui) {
        self.updates.poll(ui.ctx());
        self.poll();
        let chose_storage = self.chose_storage;
        let action = match &mut self.screen {
            Screen::Problem(details) => step(ui, "problem", false, |ui| {
                ui::step_title(ui, t!("app.problem_title"), t!("app.problem_help"));
                CollapsingHeader::new(t!("common.details"))
                    .show(ui, |ui| ui.label(details.as_str()));
                None
            }),
            Screen::Welcome => step(ui, "welcome", chose_storage, welcome),
            Screen::Words { words, .. } => step(ui, "words", true, |ui| show_words(ui, words)),
            Screen::Confirm {
                check,
                answers,
                error,
                ..
            } => step(ui, "check", true, |ui| {
                confirm(ui, check, answers, error.as_deref())
            }),
            Screen::Restore { words, error } => step(ui, "restore", true, |ui| {
                restore(ui, words, error.as_deref())
            }),
            Screen::Storage {
                password,
                error,
                busy,
                restore,
            } => {
                let back = *restore && busy.is_none();
                let busy = busy.is_some();
                let keychain = self.keystore.keychain_works();
                step(ui, "storage", back, |ui| {
                    key_storage::choose(ui, password, error, busy, keychain).map(Action::Choose)
                })
            }
            Screen::Unlock {
                password,
                error,
                tries,
                busy,
            } => {
                let busy = busy.is_some();
                step(ui, "unlock", false, |ui| {
                    key_storage::unlock(ui, password, error.as_deref(), *tries, busy).map(
                        |unlock| match unlock {
                            Unlock::Try => Action::Unlock,
                            Unlock::UseWords => Action::UseWords,
                        },
                    )
                })
            }
            Screen::Missing { keychain } => {
                let keychain = *keychain;
                step(ui, "missing", false, |ui| {
                    key_storage::missing(ui, keychain).then_some(Action::UseWords)
                })
            }
            Screen::Home(home) => {
                crate::home::show(ui, home, &mut self.updates, &mut self.keystore)
            }
        };
        if let Some(action) = action {
            self.apply(action);
        }
        crate::paste::text(ui.ctx());
    }
}

/// A first-start step in the shared layout, with Back when `back`.
fn step(
    ui: &mut Ui,
    id: &str,
    back: bool,
    add: impl FnOnce(&mut Ui) -> Option<Action>,
) -> Option<Action> {
    let (pressed, action) = ui::onboarding(ui, id, back.then(|| t!("common.back")), add);
    if pressed { Some(Action::Back) } else { action }
}

/// Plain Enter in a single-line field. Screens that focus a field when nothing has
/// focus must ask before that: Enter ends the focus, and focusing again hides it.
pub fn submitted(ui: &Ui, field: &Response) -> bool {
    field.lost_focus()
        && ui.input(|input| input.key_pressed(Key::Enter) && input.modifiers.is_none())
}

fn welcome(ui: &mut Ui) -> Option<Action> {
    ui::step_title(ui, t!("app.welcome"), t!("app.welcome_text"));
    if primary(ui, t!("app.create_account")).clicked() {
        return Some(Action::Create);
    }
    secondary(ui, t!("app.have_account"))
        .clicked()
        .then_some(Action::Restore)
}

/// When the words were last copied.
fn copied_id() -> Id {
    Id::unique("recovery words copied")
}

fn show_words(ui: &mut Ui, words: &str) -> Option<Action> {
    ui::step_title(ui, t!("app.words_title"), t!("app.words_text"));
    // Four columns don't fit a phone.
    let gap = ui::SPACE_S;
    let width = ui.available_width();
    let columns = if ui::is_phone(ui.ctx()) { 2 } else { 4 };
    let cell = (width - gap * (columns - 1) as f32) / columns as f32;
    Grid::new("recovery-words")
        .num_columns(columns)
        .spacing(Vec2::splat(gap))
        .show(ui, |ui| {
            for (index, word) in words.split_whitespace().enumerate() {
                ui::card_frame(ui)
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.set_width(cell - 20.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = ui::SPACE_XS;
                            // The number in a fixed column, centred on the word's line.
                            let height = ui.text_style_height(&egui::TextStyle::Body);
                            let (rect, _) = ui
                                .allocate_exact_size(Vec2::new(16.0, height), egui::Sense::hover());
                            ui.painter().text(
                                rect.left_center(),
                                egui::Align2::LEFT_CENTER,
                                format!("{}", index + 1),
                                egui::FontId::proportional(ui::TEXT_CAPTION),
                                ui::colors(ui).muted,
                            );
                            ui.label(ui::bold(word).size(ui::TEXT_BODY));
                        });
                    });
                if index % columns == columns - 1 {
                    ui.end_row();
                }
            }
        });
    ui.add_space(ui::SPACE_M);
    let now = ui.input(|input| input.time);
    let copied = ui
        .data(|data| data.get_temp::<f64>(copied_id()))
        .is_some_and(|at| now - at < COPIED_SECONDS);
    ui.horizontal(|ui| {
        // The button says it worked for a moment; a password manager is the tip.
        let label = if copied {
            t!("app.copied")
        } else {
            t!("clipboard.copy")
        };
        if ui::secondary(ui, label).clicked() {
            words::copy(ui.ctx(), words);
            ui.data_mut(|data| data.insert_temp(copied_id(), now));
            ui.ctx().request_repaint_after_secs(COPIED_SECONDS as f32);
        }
    });
    ui.label(ui::caption(t!("app.words_manager")));
    ui.add_space(ui::SPACE_L);
    primary(ui, t!("app.wrote_them"))
        .clicked()
        .then_some(Action::Continue)
}

/// A field's label with a Paste button at the other end; true when it was pressed.
fn label_with_paste(ui: &mut Ui, label: &str) -> (Response, bool) {
    ui.horizontal(|ui| {
        ui.set_min_height(ui::CONTROL_HEIGHT);
        let label = ui.label(label);
        let pasted = ui
            .with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui::secondary(ui, t!("clipboard.paste")).clicked()
            })
            .inner;
        (label, pasted)
    })
    .inner
}

fn word_field_id(slot: usize) -> Id {
    Id::unique(("word check", slot))
}

fn restore_field_id() -> Id {
    Id::unique("recovery words field")
}

/// Remove what egui keeps of the recovery-word fields (cursor and text layout),
/// as `key_storage::forget` does for passwords.
fn forget_words(ctx: &egui::Context) {
    let ids = (0..3).map(word_field_id).chain([restore_field_id()]);
    ctx.data_mut(|data| {
        for id in ids {
            data.remove::<egui::text_edit::TextEditState>(id);
        }
    });
}

fn confirm(
    ui: &mut Ui,
    check: &WordConfirmation,
    answers: &mut [Zeroizing<String>; 3],
    error: Option<&str>,
) -> Option<Action> {
    ui::step_title(ui, t!("app.check_title"), t!("app.check_text"));
    let indices = check.indices();
    // The field last typed in: a paste of a few words starts there.
    let last_id = Id::unique("word check last field");
    let focused = (0..answers.len()).find(|&slot| ui.memory(|m| m.has_focus(word_field_id(slot))));
    if let Some(slot) = focused {
        ui.data_mut(|data| data.insert_temp(last_id, slot));
    }
    if let Some(pasted) = words::pasted(ui) {
        let start = ui
            .data(|data| data.get_temp::<usize>(last_id))
            .or_else(|| answers.iter().position(|answer| answer.is_empty()))
            .unwrap_or(0);
        words::fill(answers, &indices, &pasted, start);
    }
    let mut done = false;
    for (slot, (answer, index)) in answers.iter_mut().zip(indices).enumerate() {
        let text = t!("app.word_number", number = index + 1);
        let label = if slot == 0 {
            let (label, paste) = label_with_paste(ui, &text);
            if paste {
                words::request_paste(ui.ctx());
            }
            label
        } else {
            ui.label(text)
        };
        let field = ui
            .add(
                ui::field(&mut **answer)
                    .id(word_field_id(slot))
                    .desired_width(f32::INFINITY)
                    .private(),
            )
            .labelled_by(label.id);
        done |= submitted(ui, &field);
        if slot == 0 {
            ui::focus_when_none(ui, &field);
        }
        ui.add_space(ui::SPACE_XS);
    }
    error_text(ui, error);
    ui.add_space(ui::SPACE_M);
    (primary(ui, t!("app.confirm")).clicked() || done).then_some(Action::Continue)
}

fn restore(ui: &mut Ui, words: &mut Zeroizing<String>, error: Option<&str>) -> Option<Action> {
    ui::step_title(ui, t!("app.restore_title"), t!("app.restore_text"));
    if let Some(pasted) = words::pasted(ui) {
        words::append(words, &pasted, WORD_COUNT);
    }
    let (label, paste) = label_with_paste(ui, t!("app.recovery_words"));
    if paste {
        words::request_paste(ui.ctx());
    }
    let field = ui
        .add(
            ui::text_area(&mut **words)
                .id(restore_field_id())
                .desired_rows(4)
                .desired_width(f32::INFINITY)
                .private(),
        )
        .labelled_by(label.id);
    ui::focus_when_none(ui, &field);
    error_text(ui, error);
    ui.add_space(ui::SPACE_M);
    primary(ui, t!("app.sign_in"))
        .clicked()
        .then_some(Action::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn recovery_words_never_reach_the_keyboard() {
        use crate::android::{Bridge, cursor, set_cursor, start};
        let ctx = egui::Context::default();
        let mut words = Zeroizing::new("apple banana cherry".to_owned());
        let frame = |words: &mut Zeroizing<String>| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                restore(ui, words, None);
            });
            output.textures_delta.clear();
        };
        // The screen focuses its field, which then shares itself with the keyboard.
        frame(&mut words);
        frame(&mut words);
        let field = ctx
            .memory(|memory| memory.focused())
            .expect("focused field");
        // What would go to GameTextInput_setState.
        let sent = |bridge: &Bridge| bridge.buffer().to_java().0;
        let mut bridge = Bridge::default();
        let at = start(&ctx, &mut bridge, field, cursor(&ctx, field).unwrap(), true);
        assert_eq!(at, (19, 19), "typing starts at the end");
        assert_eq!(&sent(&bridge)[..], b"\0");
        // Moving the cursor into the words starts over, still without them.
        set_cursor(&ctx, field, (6, 6));
        frame(&mut words);
        let at = start(
            &ctx,
            &mut bridge,
            field,
            cursor(&ctx, field).unwrap(),
            false,
        );
        assert_eq!(at, (6, 6));
        assert_eq!(&sent(&bridge)[..], b"\0");
    }

    #[test]
    fn typed_recovery_words_leave_no_undo_history_or_state() {
        use egui::text_edit::TextEditState;
        let ctx = egui::Context::default();
        let mut words = key_storage::secret_input();
        for (time, text) in [(0.0, ""), (0.1, "apple"), (5.0, " banana"), (10.0, "")] {
            let events = if text.is_empty() {
                Vec::new()
            } else {
                vec![egui::Event::Text(text.into())]
            };
            let raw = egui::RawInput {
                time: Some(time),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                restore(ui, &mut words, None);
            });
            output.textures_delta.clear();
        }
        assert_eq!(*words, "apple banana");
        let state = TextEditState::load(&ctx, restore_field_id()).expect("field state");
        // Ctrl+Z would have nothing to bring back.
        assert!(
            !state
                .undoer()
                .has_undo(&(Default::default(), String::new()))
        );
        // Leaving the screen (Back, or on to the next one) forgets the field.
        forget_words(&ctx);
        assert!(TextEditState::load(&ctx, restore_field_id()).is_none());
    }

    #[test]
    fn back_keeps_the_words_and_leaves_no_account_behind() {
        let (words, identity) = DeviceIdentity::create(DEVICE_NAME).unwrap();
        let shown = words.to_string();
        let check = Screen::Confirm {
            words,
            identity,
            check: WordConfirmation::random().unwrap(),
            answers: std::array::from_fn(|_| key_storage::secret_input()),
            error: None,
        };
        // From the word check, the same words again.
        let screen = back(check, false, None);
        let Screen::Words { words, .. } = &screen else {
            panic!("back to the words");
        };
        assert_eq!(**words, shown);
        // Before them, the words (never saved) are dropped.
        assert!(matches!(back(screen, false, None), Screen::Welcome));

        let restore = || Screen::Restore {
            words: key_storage::secret_input(),
            error: None,
        };
        assert!(matches!(back(restore(), true, None), Screen::Welcome));
        assert!(matches!(
            back(Screen::Welcome, true, None),
            Screen::Storage { restore: false, .. }
        ));
        assert!(matches!(
            back(Screen::Welcome, false, None),
            Screen::Welcome
        ));
        // Asked for from the missing-keys screen: back through the storage choice.
        let missing = Some(Recover::Missing { keychain: true });
        let storage = back(restore(), false, missing);
        assert!(matches!(storage, Screen::Storage { restore: true, .. }));
        assert!(matches!(
            back(storage, false, missing),
            Screen::Missing { keychain: true }
        ));
        let storage = back(restore(), false, Some(Recover::Unlock));
        assert!(matches!(
            back(storage, false, Some(Recover::Unlock)),
            Screen::Unlock { .. }
        ));
    }

    #[test]
    fn sign_out_removes_account_data_and_preserves_device_setup() {
        let profile = tempfile::tempdir().unwrap();
        let dir = profile.path();
        let account = [
            "sync.json",
            "servers.json",
            "saved.json",
            "notify.json",
            "pokes.json",
            "dm_deleted.json",
            "emoji-recent.json",
            "emoji-favourites.json",
            "rusttalk.log",
            "rusttalk.log.1",
        ];
        let device = [
            "identity.lock",
            "servers.lock",
            "updates.json",
            "deepfilternet3.tar.gz",
            "rusttalk-update.exe",
        ];
        for file in account.into_iter().chain(device) {
            fs::write(dir.join(file), b"original").unwrap();
        }
        for file in [
            "dm_pins/server.json",
            "previews/card.json",
            "previews/card.img",
        ] {
            let path = dir.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"private").unwrap();
        }
        let voice = crate::audio::Settings {
            input: Some("desk mic".into()),
            dismissed_official: vec!["private server".into()],
            friends_only: true,
            create_link_previews: Some(true),
            ..Default::default()
        };
        rusttalk_core::client::save_json(dir, &dir.join("voice.json"), &voice).unwrap();
        forget_profile(dir, voice);
        for file in account.into_iter().chain(["dm_pins", "previews"]) {
            assert!(!dir.join(file).exists(), "{file} survived sign-out");
        }
        for file in device {
            assert_eq!(fs::read(dir.join(file)).unwrap(), b"original", "{file}");
        }
        let clean: crate::audio::Settings = crate::load_json(&dir.join("voice.json")).unwrap();
        assert!(
            clean
                == crate::audio::Settings {
                    input: Some("desk mic".into()),
                    ..Default::default()
                }
        );
    }
}
