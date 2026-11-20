use anyhow::Result;
use eframe::egui::{
    self, Button, CentralPanel, CollapsingHeader, Frame, Grid, Id, Key, Modal, Panel, Response,
    RichText, Role, ScrollArea, TextEdit, TextStyle, Ui, Vec2, WidgetInfo,
};
use rusttalk_core::{
    auth::InviteLink,
    identity::{DeviceIdentity, IdentityError, WordConfirmation, fingerprint},
};
use zeroize::Zeroizing;

use crate::{keystore::Keystore, server::Server};

const DEVICE_NAME: &str = "Desktop";
const CARD_WIDTH: f32 = 560.0;

pub struct App {
    /// `None` only when startup failed and the problem screen is showing.
    keystore: Option<Keystore>,
    screen: Screen,
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

enum Action {
    Create,
    Restore,
    StartOver,
    Continue,
    SignOut,
}

struct Home {
    identity: DeviceIdentity,
    servers: Vec<Server>,
    view: View,
    invite: String,
    invite_note: Option<String>,
    settings: Option<Tab>,
    confirm_sign_out: bool,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Join,
    Direct,
    Server(usize),
    /// Server, category and channel index.
    Channel(usize, usize, usize),
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Profile,
    Devices,
}

impl Home {
    fn new(identity: DeviceIdentity) -> Self {
        Self {
            identity,
            servers: Vec::new(),
            view: View::Join,
            invite: String::new(),
            invite_note: None,
            settings: None,
            confirm_sign_out: false,
            error: None,
        }
    }
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext,
        opened: Result<(Keystore, Option<DeviceIdentity>)>,
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
        match opened {
            Ok((keystore, identity)) => Self {
                keystore: Some(keystore),
                screen: identity.map_or(Screen::Welcome, |identity| {
                    Screen::Home(Box::new(Home::new(identity)))
                }),
            },
            Err(error) => Self {
                keystore: None,
                screen: Screen::Problem(format!("{error:#}")),
            },
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
                    None => Screen::Home(Box::new(Home::new(identity))),
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
                            self.screen = Screen::Home(Box::new(Home::new(identity)));
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
            Screen::Home(home) => home_ui(ui, home),
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

fn primary(ui: &mut Ui, text: &str) -> Response {
    ui.add(
        Button::new(RichText::new(text).size(16.0))
            .fill(ui.visuals().selection.bg_fill)
            .min_size(Vec2::new(240.0, 40.0)),
    )
}

fn secondary(ui: &mut Ui, text: &str) -> Response {
    ui.add(Button::new(RichText::new(text).size(16.0)).min_size(Vec2::new(240.0, 40.0)))
}

fn error_text(ui: &mut Ui, error: Option<&str>) {
    if let Some(error) = error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
}

fn submitted(ui: &Ui, field: &Response) -> bool {
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

fn home_ui(ui: &mut Ui, home: &mut Home) -> Option<Action> {
    Panel::left("servers")
        .resizable(false)
        .exact_size(72.0)
        .show(ui, |ui| server_rail(ui, home));
    Panel::left("channels")
        .resizable(false)
        .exact_size(240.0)
        .show(ui, |ui| channel_list(ui, home));
    if let View::Server(server) | View::Channel(server, ..) = home.view {
        Panel::right("members")
            .resizable(false)
            .exact_size(220.0)
            .show(ui, |ui| member_list(ui, &home.servers[server]));
    }
    CentralPanel::default().show(ui, |ui| main_pane(ui, home));
    settings(ui.ctx(), home)
}

fn round_button(ui: &mut Ui, text: &str, label: &str, selected: bool) -> Response {
    let response = ui
        .add(
            Button::new(RichText::new(text).size(20.0))
                .min_size(Vec2::splat(48.0))
                .corner_radius(if selected { 16 } else { 24 })
                .selected(selected),
        )
        .on_hover_text(label);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    response
}

fn server_rail(ui: &mut Ui, home: &mut Home) {
    ui.vertical_centered(|ui| {
        ui.add_space(8.0);
        for (index, server) in home.servers.iter().enumerate() {
            let selected =
                matches!(home.view, View::Server(s) | View::Channel(s, ..) if s == index);
            let initial = server
                .name
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            if round_button(ui, &initial, &server.name, selected).clicked() {
                home.view = View::Server(index);
            }
        }
        if !home.servers.is_empty() {
            ui.separator();
        }
        if round_button(ui, "+", "Add a server", home.view == View::Join).clicked() {
            home.view = View::Join;
        }
    });
}

fn channel_list(ui: &mut Ui, home: &mut Home) {
    Panel::bottom("you").show(ui, |ui| {
        ui.add_space(6.0);
        if ui.button("⚙  Settings").clicked() {
            home.settings = Some(Tab::Profile);
        }
        ui.add_space(2.0);
    });
    ui.add_space(8.0);
    if ui
        .selectable_label(home.view == View::Direct, "Direct messages")
        .clicked()
    {
        home.view = View::Direct;
    }
    let (View::Server(index) | View::Channel(index, ..)) = home.view else {
        return;
    };
    ui.separator();
    ui.heading(&home.servers[index].name);
    ScrollArea::vertical().show(ui, |ui| {
        for (c, category) in home.servers[index].categories.iter().enumerate() {
            if !category.name.is_empty() {
                ui.add_space(8.0);
                ui.label(RichText::new(category.name.to_uppercase()).small().weak());
            }
            for (n, channel) in category.channels.iter().enumerate() {
                let icon = if channel.voice { "🔊" } else { "#" };
                let view = View::Channel(index, c, n);
                if ui
                    .selectable_label(home.view == view, format!("{icon} {}", channel.name))
                    .clicked()
                {
                    home.view = view;
                }
            }
        }
    });
}

fn member_list(ui: &mut Ui, server: &Server) {
    ui.add_space(8.0);
    ui.label(
        RichText::new(format!("MEMBERS — {}", server.members.len()))
            .small()
            .weak(),
    );
    ScrollArea::vertical().show(ui, |ui| {
        for member in &server.members {
            ui.label(member);
        }
    });
}

fn main_pane(ui: &mut Ui, home: &mut Home) {
    match home.view {
        View::Join => join(ui, home),
        View::Direct => {
            ui.heading("Direct messages");
            ui.separator();
            ui.weak("No messages yet.");
        }
        View::Server(server) => {
            ui.heading(&home.servers[server].name);
            ui.separator();
            ui.weak("Pick a channel on the left.");
        }
        View::Channel(server, category, channel) => {
            let channel = &home.servers[server].categories[category].channels[channel];
            let icon = if channel.voice { "🔊" } else { "#" };
            ui.heading(format!("{icon} {}", channel.name));
            ui.separator();
        }
    }
}

fn join(ui: &mut Ui, home: &mut Home) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.2);
        ui.heading(if home.servers.is_empty() {
            "No servers yet"
        } else {
            "Join a server"
        });
        ui.label("Paste an invite link from a friend.");
        ui.add_space(12.0);
        let label = ui.label("Invite link");
        let field = ui
            .add(
                TextEdit::singleline(&mut home.invite)
                    .hint_text("rusttalk://…")
                    .desired_width(400.0),
            )
            .labelled_by(label.id);
        if primary(ui, "Join a server").clicked() || submitted(ui, &field) {
            home.invite_note = Some(match InviteLink::parse(home.invite.trim()) {
                Ok(invite) => format!(
                    "Found the server at {}. Joining isn't ready in this version yet.",
                    invite.server.address.host_str().unwrap_or_default()
                ),
                Err(error) => {
                    tracing::warn!(%error, "invite link rejected");
                    "That isn't an invite link. Ask your friend to send it again.".into()
                }
            });
        }
        if let Some(note) = &home.invite_note {
            ui.label(note.as_str());
        }
    });
}

fn settings(ctx: &egui::Context, home: &mut Home) -> Option<Action> {
    let mut tab = home.settings?;
    let mut close = false;
    let modal = Modal::new(Id::unique("settings")).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading("Settings");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut tab, Tab::Profile, "Profile");
            ui.selectable_value(&mut tab, Tab::Devices, "Devices");
        });
        ui.separator();
        match tab {
            Tab::Profile => {
                ui.label("You pick your name on each server when you join it.");
                CollapsingHeader::new("Details").show(ui, |ui| {
                    ui.label("Your ID");
                    ui.monospace(fingerprint(home.identity.certificate.person_id()));
                    ui.weak("Friends can compare this to make sure it's really you.");
                });
            }
            Tab::Devices => {
                ui.label(format!("This device: {}", home.identity.certificate.name()));
            }
        }
        error_text(ui, home.error.as_deref());
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Sign out of this device").clicked() {
                home.confirm_sign_out = true;
            }
            close = ui.button("Close").clicked();
        });
    });
    home.settings = Some(tab);
    if close || (modal.should_close() && !home.confirm_sign_out) {
        home.settings = None;
        home.error = None;
    }

    if !home.confirm_sign_out {
        return None;
    }
    let mut action = None;
    let mut cancel = false;
    let modal = Modal::new(Id::unique("sign-out")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.heading("Sign out of this device?");
        ui.label("This removes your account from this computer. To sign in again, you need your 24 recovery words.");
        ui.horizontal(|ui| {
            if ui.button("Sign out").clicked() {
                action = Some(Action::SignOut);
            }
            cancel = ui.button("Cancel").clicked();
        });
    });
    if cancel || modal.should_close() {
        home.confirm_sign_out = false;
    }
    action
}
