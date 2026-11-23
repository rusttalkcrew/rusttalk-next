use std::{cell::Cell, path::PathBuf};

use eframe::egui::{
    self, CentralPanel, CollapsingHeader, Grid, Key, Response, ScrollArea, Ui, Vec2,
};
use rusttalk_core::identity::{DeviceIdentity, IdentityError, WordConfirmation};
use zeroize::Zeroizing;

use crate::{
    home::Home,
    key_storage::{self, NewPassword, Picked, Unlock},
    keystore::{Chosen, Job, Keystore, Kind, Sealer, Start, Unlocked},
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
    StartOver,
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
        let screen = std::mem::replace(&mut self.screen, Screen::Welcome);
        self.screen = match (action, screen) {
            (Action::StartOver, _) => Screen::Welcome,
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
            (Action::UseWords, _) => {
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

fn problem(what: &str, error: &IdentityError) -> Screen {
    tracing::error!(%error, "{what} failed");
    Screen::Problem(format!("{what}: {error}"))
}

impl eframe::App for App<'_> {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
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

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "android")]
        crate::android::frame(ui, |ui| self.screens(ui));
        #[cfg(not(target_os = "android"))]
        self.screens(ui);
        #[cfg(any(target_os = "android", target_os = "ios"))]
        crate::phone_clipboard::frame_end(ui.ctx());
    }
}

impl App<'_> {
    fn screens(&mut self, ui: &mut Ui) {
        self.updates.poll(ui.ctx());
        self.poll();
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
            Screen::Storage {
                password,
                error,
                busy,
                ..
            } => {
                let busy = busy.is_some();
                card(ui, |ui| {
                    key_storage::choose(ui, password, error, busy).map(Action::Choose)
                })
            }
            Screen::Unlock {
                password,
                error,
                tries,
                busy,
            } => {
                let busy = busy.is_some();
                card(ui, |ui| {
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
                card(ui, |ui| {
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

/// Plain Enter in a single-line field. Screens that focus a field when nothing has
/// focus must ask before that: Enter ends the focus, and focusing again hides it.
pub fn submitted(ui: &Ui, field: &Response) -> bool {
    field.lost_focus()
        && ui.input(|input| input.key_pressed(Key::Enter) && input.modifiers.is_none())
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
    // Four columns don't fit a phone.
    let columns = if ui::is_phone(ui.ctx()) { 2 } else { 4 };
    Grid::new("recovery-words")
        .num_columns(columns)
        .spacing(Vec2::new(10.0, 10.0))
        .show(ui, |ui| {
            for (index, word) in words.split_whitespace().enumerate() {
                ui::card_frame(ui).show(ui, |ui| {
                    ui.set_width(110.0);
                    ui.label(ui::caption(format!("{}", index + 1)));
                    ui.label(ui::bold(word).size(16.0));
                });
                if index % columns == columns - 1 {
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
            .add(ui::field(&mut **answer).desired_width(260.0).private())
            .labelled_by(label.id);
        done |= submitted(ui, &field);
        if slot == 0 && ui.memory(|memory| memory.focused().is_none()) {
            field.request_focus();
        }
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
                .desired_width(f32::INFINITY)
                .private(),
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
