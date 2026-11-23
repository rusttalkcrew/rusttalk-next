//! Calling a phone into the voice room: the button in the call bar, the dialer with
//! contacts and a keypad, the "End the call?" question before leaving, and the SIP
//! account page in Settings. Nothing shows until an account is saved. Desktop only.

use std::time::Instant;

use eframe::egui::{self, Align, Key, Layout, Ui, Vec2};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::{Voice, connect, leave};
use crate::{
    audio::Settings,
    home::Home,
    net::Command,
    phone::{
        self, Account, Contact, KEYS,
        call::{Control, Dial, Event, Status},
    },
    server::Id,
    ui::{self, Look, SPACE_S, SPACE_XS, error_text},
};

const KEY_SIZE: Vec2 = Vec2::new(72.0, 44.0);
/// Shown in front of the call's name, so everyone sees it is a phone.
const MARK: &str = "📞";

#[derive(Default)]
pub struct Phone {
    /// The dialer is open.
    open: bool,
    tab: Tab,
    /// What the keypad typed.
    typed: String,
    pub call: Option<Active>,
    /// Why the last call failed, until the next one.
    error: Option<String>,
    /// Leaving waits for "End the call?".
    confirm: Option<Leave>,
    /// A contact being added (`None`) or changed (its index).
    editing: Option<(Option<usize>, Contact)>,
    /// The account page's fields, from Settings once it opens.
    draft: Option<Draft>,
    /// A password is saved; asked once, the keychain is slow.
    has_password: Option<bool>,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Tab {
    #[default]
    Contacts,
    Keypad,
}

pub struct Active {
    generation: u64,
    label: String,
    status: Status,
    /// When it was answered, for the timer.
    since: Option<Instant>,
    control: UnboundedSender<Control>,
}

#[derive(Clone, Copy)]
pub enum Leave {
    Leave,
    Join(usize, Id),
}

struct Draft {
    account: Account,
    password: zeroize::Zeroizing<String>,
    note: Option<&'static str>,
}

impl Phone {
    fn has_password(&mut self) -> bool {
        *self
            .has_password
            .get_or_insert_with(|| phone::password().is_some())
    }

    /// An account is set up, so the phone shows.
    fn ready(&mut self, settings: &Settings) -> bool {
        settings.phone.filled() && self.has_password()
    }

    fn hang_up(&mut self) {
        if let Some(call) = self.call.take() {
            let _ = call.control.send(Control::HangUp);
        }
    }

    /// The call was left: the network side hangs up with the room.
    pub fn end(&mut self) {
        self.call = None;
        self.open = false;
        self.confirm = None;
    }
}

/// How the call is going, from the network side.
pub fn update(phone: &mut Phone, generation: u64, event: Event) {
    if !phone
        .call
        .as_ref()
        .is_some_and(|call| call.generation == generation)
    {
        return;
    }
    match event {
        Event::Status(status) => {
            if let Some(call) = &mut phone.call {
                call.status = status;
                if status == Status::Talking {
                    call.since.get_or_insert_with(Instant::now);
                }
            }
        }
        Event::Ended(why) => {
            phone.call = None;
            // Show why it failed, even when the dialer was closed.
            phone.open |= why.is_some();
            phone.error = why;
        }
    }
}

/// Leave the call, or ask first while a phone is in it.
pub fn leave_or_ask(home: &mut Home, then: Leave) -> bool {
    if home.voice.phone.call.is_none() {
        return false;
    }
    home.voice.phone.confirm = Some(then);
    true
}

/// The phone button in the call bar; nothing without an account.
pub fn button(ui: &mut Ui, voice: &mut Voice, enabled: bool, size: Vec2) -> bool {
    let settings = voice.audio.settings();
    let phone = &mut voice.phone;
    if !phone.ready(&settings) {
        return false;
    }
    let look = if phone.call.is_some() {
        Look::On(ui::colors(ui).success)
    } else {
        Look::Plain
    };
    let button = ui
        .add_enabled_ui(enabled || phone.call.is_some(), |ui| {
            ui::icon_button(ui, MARK, t!("phone.call_a_phone"), look, size)
        })
        .inner;
    if button.clicked() {
        phone.open ^= true;
    }
    if phone.call.is_some() {
        ui::context_menu(&button, |m| {
            if m.danger(MARK, t!("phone.hang_up")).clicked() {
                phone.hang_up();
            }
        });
    }
    true
}

/// Whether the phone button shows, so the call bar can make room for it.
pub fn shows(voice: &mut Voice) -> bool {
    let settings = voice.audio.settings();
    voice.phone.ready(&settings)
}

/// The dialer and the leave question, every frame.
pub fn windows(ctx: &egui::Context, home: &mut Home) {
    confirm(ctx, home);
    if !home.voice.phone.open {
        return;
    }
    if home.voice.call.is_none() {
        home.voice.phone.open = false;
        return;
    }
    let title = match &home.voice.phone.call {
        Some(call) => call.label.clone(),
        None => t!("phone.call_a_phone").to_owned(),
    };
    let mut dial = None;
    let ((), close) = ui::dialog(ctx, "phone-dialer", &title, |ui| {
        let phone = &mut home.voice.phone;
        if phone.call.is_some() {
            in_call(ui, phone);
        } else {
            let mut settings = home.voice.audio.settings();
            dial = dialer(ui, phone, &mut settings);
            if settings != home.voice.audio.settings() {
                home.voice.audio.set_settings(settings);
            }
        }
        ui.add_space(SPACE_S);
        ui.label(ui::caption(t!("phone.not_encrypted")));
    });
    if close {
        let phone = &mut home.voice.phone;
        phone.open = false;
        phone.editing = None;
    }
    if let Some(typed) = dial {
        start(home, &typed);
    }
}

/// The call: who, how long, the keypad for menus, and hang up.
fn in_call(ui: &mut Ui, phone: &mut Phone) {
    let Some(call) = &phone.call else { return };
    let status = match (call.status, call.since) {
        (Status::Calling, _) => t!("phone.calling").to_owned(),
        (Status::Ringing, _) => t!("phone.ringing").to_owned(),
        (Status::Talking, Some(since)) => {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
            let secs = since.elapsed().as_secs();
            format!("{}:{:02}", secs / 60, secs % 60)
        }
        (Status::Talking, None) => String::new(),
    };
    ui.label(ui::caption(status));
    ui.add_space(SPACE_S);
    if call.status == Status::Talking
        && let Some(key) = keypad(ui)
    {
        let _ = call.control.send(Control::Key(key));
    }
    ui::footer(ui, |ui| {
        if ui::danger(ui, t!("phone.hang_up")).clicked() {
            phone.hang_up();
        }
    });
}

/// Twelve keys in four rows; the one pressed, also from the keyboard.
fn keypad(ui: &mut Ui) -> Option<char> {
    let mut pressed = None;
    let width = 3.0 * KEY_SIZE.x + 2.0 * SPACE_XS;
    ui.vertical_centered(|ui| {
        ui.set_max_width(width);
        ui.spacing_mut().item_spacing = Vec2::splat(SPACE_XS);
        for row in KEYS.chunks(3) {
            ui.horizontal(|ui| {
                for &key in row {
                    let text = key.to_string();
                    if ui::icon_button(ui, &text, &text, Look::Plain, KEY_SIZE).clicked() {
                        pressed = Some(key);
                    }
                }
            });
        }
    });
    if pressed.is_none() && ui.ctx().memory(|m| m.focused().is_none()) {
        pressed = ui.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Text(text) => text.chars().next().filter(|c| KEYS.contains(c)),
                _ => None,
            })
        });
    }
    pressed
}

/// Contacts or the keypad. Returns what to call.
fn dialer(ui: &mut Ui, phone: &mut Phone, settings: &mut Settings) -> Option<String> {
    ui::segmented(
        ui,
        &mut phone.tab,
        &[
            (Tab::Contacts, t!("phone.contacts"), ""),
            (Tab::Keypad, t!("phone.keypad"), ""),
        ],
    );
    ui.add_space(SPACE_S);
    let dial = match phone.tab {
        Tab::Contacts => contacts(ui, phone, &mut settings.phone_contacts),
        Tab::Keypad => {
            let field = ui.add(
                ui::field(&mut phone.typed)
                    .hint_text(t!("phone.number_hint"))
                    .desired_width(ui.available_width()),
            );
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.add_space(SPACE_S);
            if let Some(key) = keypad(ui) {
                phone.typed.push(key);
            }
            let mut dial = None;
            ui::footer(ui, |ui| {
                let can = !phone.typed.trim().is_empty();
                let call = ui
                    .add_enabled_ui(can, |ui| ui::primary(ui, t!("phone.call")))
                    .inner;
                if can && (call.clicked() || enter) {
                    dial = Some(phone.typed.clone());
                }
                if !phone.typed.is_empty() && ui::secondary(ui, "⌫").clicked() {
                    phone.typed.pop();
                }
            });
            dial
        }
    };
    error_text(ui, phone.error.as_deref());
    dial
}

/// The contact list with a call button each, and adding, changing and removing them.
fn contacts(ui: &mut Ui, phone: &mut Phone, list: &mut Vec<Contact>) -> Option<String> {
    if let Some((at, contact)) = &mut phone.editing {
        ui.add(ui::field(&mut contact.name).hint_text(t!("phone.contact_name")));
        ui.add_space(SPACE_XS);
        ui.add(ui::field(&mut contact.number).hint_text(t!("phone.number_hint")));
        let mut done = false;
        ui::footer(ui, |ui| {
            let ok = phone::clean_number(&contact.number).is_some() || contact.number.contains('@');
            if ui
                .add_enabled_ui(ok, |ui| ui::primary(ui, t!("common.save")))
                .inner
                .clicked()
            {
                let saved = Contact {
                    name: contact.name.trim().to_owned(),
                    number: contact.number.trim().to_owned(),
                };
                match at.and_then(|at| list.get_mut(at)) {
                    Some(old) => *old = saved,
                    None => list.push(saved),
                }
                list.sort_by_key(|c| c.name.to_lowercase());
                done = true;
            }
            done |= ui::secondary(ui, t!("common.cancel")).clicked();
        });
        if done {
            phone.editing = None;
        }
        return None;
    }
    if list.is_empty() {
        ui.weak(t!("phone.no_contacts"));
    }
    let mut dial = None;
    let mut remove = None;
    for (at, contact) in list.iter().enumerate() {
        let name = if contact.name.is_empty() {
            &contact.number
        } else {
            &contact.name
        };
        // The call button on the right; the row fills what is left, top down (a
        // right-to-left row has no left edge, which made the row NaN wide).
        let mut call = false;
        let trailing = |ui: &mut Ui| {
            call = ui::icon_button(
                ui,
                MARK,
                t!("phone.call"),
                Look::Ghost,
                Vec2::splat(ui::ICON_BUTTON),
            )
            .clicked();
        };
        ui::trailing_row(ui, trailing, |ui| {
            let row = ui::row_button(ui, ui::ROW_HEIGHT, name, |ui| {
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(ui::bold(name)).truncate());
                    if !contact.name.is_empty() {
                        ui.add(egui::Label::new(ui::caption(&contact.number)).truncate());
                    }
                });
            });
            if row.clicked() {
                dial = Some(contact.number.clone());
            }
            ui::context_menu(&row, |m| {
                if m.item("✏", t!("phone.edit_contact")).clicked() {
                    phone.editing = Some((Some(at), contact.clone()));
                }
                if m.danger("🗑", t!("phone.remove_contact")).clicked() {
                    remove = Some(at);
                }
            });
        });
        if call {
            dial = Some(contact.number.clone());
        }
    }
    if let Some(at) = remove {
        list.remove(at);
    }
    ui.add_space(SPACE_XS);
    if ui::secondary(ui, t!("phone.add_contact")).clicked() {
        phone.editing = Some((None, Contact::default()));
    }
    dial
}

/// Start calling `typed` from the call we are in.
fn start(home: &mut Home, typed: &str) {
    let settings = home.voice.audio.settings();
    let phone = &mut home.voice.phone;
    phone.error = None;
    let Some(target) = phone::target(typed, &settings.phone) else {
        phone.error = Some(t!("phone.bad_number").to_owned());
        return;
    };
    let Some(password) = phone::password() else {
        phone.has_password = Some(false);
        phone.error = Some(t!("phone.no_password").to_owned());
        return;
    };
    let Some(server) = (home.voice.call.as_ref())
        .and_then(|call| home.servers.iter().find(|s| s.address == call.server))
    else {
        return;
    };
    let name = phone::label(typed, &settings.phone_contacts);
    let label = format!("{MARK} {name}");
    let (control, receiver) = unbounded_channel();
    let generation = phone::call::next_generation();
    server.send(Command::PhoneCall(Box::new(Dial {
        generation,
        account: settings.phone.clone(),
        password,
        target,
        label: label.clone(),
        control: receiver,
    })));
    phone.call = Some(Active {
        generation,
        label,
        status: Status::Calling,
        since: None,
        control,
    });
    phone.typed.clear();
}

/// "You're on a phone call": end it and leave, or stay.
fn confirm(ctx: &egui::Context, home: &mut Home) {
    let Some(then) = home.voice.phone.confirm else {
        return;
    };
    let mut answer = None;
    let ((), dismissed) = ui::dialog(ctx, "phone-leave", t!("phone.leave_title"), |ui| {
        ui.label(t!("phone.leave_help"));
        ui::footer(ui, |ui| {
            if ui::danger(ui, t!("phone.leave_and_hang_up")).clicked() {
                answer = Some(true);
            }
            if ui::secondary(ui, t!("common.cancel")).clicked() {
                answer = Some(false);
            }
        });
    });
    match answer {
        Some(true) => {
            home.voice.phone.hang_up();
            home.voice.phone.confirm = None;
            match then {
                Leave::Leave => leave(home),
                Leave::Join(index, channel) => {
                    leave(home);
                    connect(home, index, channel);
                }
            }
        }
        Some(false) => home.voice.phone.confirm = None,
        None if dismissed => home.voice.phone.confirm = None,
        None => {}
    }
}

/// Settings > Phone: the SIP account. Saving keeps the password in the keychain.
pub fn settings_page(ui: &mut Ui, voice: &mut Voice) {
    let mut settings = voice.audio.settings();
    let phone = &mut voice.phone;
    let saved = phone.has_password();
    let draft = phone.draft.get_or_insert_with(|| Draft {
        account: settings.phone.clone(),
        password: zeroize::Zeroizing::new(String::new()),
        note: None,
    });
    ui::page_section(ui, t!("phone.account"));
    ui.label(t!("phone.account_help"));
    ui.add_space(SPACE_S);
    let width = ui.available_width().min(360.0);
    let row = |ui: &mut Ui, label: &str, help: &str, text: &mut String, password: bool| {
        ui::setting_row(ui, label, help, |ui| {
            let field = ui::field(text).desired_width(width);
            ui.add(if password { field.password() } else { field });
        });
    };
    row(
        ui,
        t!("phone.server"),
        t!("phone.server_help"),
        &mut draft.account.server,
        false,
    );
    row(ui, t!("phone.user"), "", &mut draft.account.user, false);
    let hint = if saved {
        t!("phone.password_saved")
    } else {
        ""
    };
    row(ui, t!("phone.password"), hint, &mut draft.password, true);
    row(
        ui,
        t!("phone.caller_name"),
        t!("phone.caller_name_help"),
        &mut draft.account.name,
        false,
    );
    let mut save = false;
    let mut remove = false;
    ui::footer(ui, |ui| {
        let filled = draft.account.filled() && (!draft.password.is_empty() || saved);
        save = ui
            .add_enabled_ui(filled, |ui| ui::primary(ui, t!("common.save")))
            .inner
            .clicked();
        if settings.phone.filled() {
            remove = ui::danger(ui, t!("phone.remove_account")).clicked();
        }
    });
    if save {
        let kept = draft.password.is_empty() || phone::save_password(&draft.password);
        draft.password = zeroize::Zeroizing::new(String::new());
        settings.phone = Account {
            server: draft.account.server.trim().to_owned(),
            user: draft.account.user.trim().to_owned(),
            name: draft.account.name.trim().to_owned(),
        };
        draft.account = settings.phone.clone();
        draft.note = Some(if kept {
            t!("phone.saved")
        } else {
            t!("phone.saved_until_restart")
        });
        phone.has_password = None;
        voice.audio.set_settings(settings);
    } else if remove {
        phone::forget_password();
        settings.phone = Account::default();
        *draft = Draft {
            account: Account::default(),
            password: zeroize::Zeroizing::new(String::new()),
            note: Some(t!("phone.removed")),
        };
        phone.has_password = Some(false);
        voice.audio.set_settings(settings);
    }
    if let Some(note) = draft.note {
        ui.label(ui::caption(note));
    }
}

/// Leaving the page drops what was typed but not saved.
pub fn leave_settings(voice: &mut Voice) {
    voice.phone.draft = None;
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    /// The dialer as `windows` draws it, without the rest of the app.
    fn harness(
        phone: Phone,
        settings: Settings,
    ) -> Harness<'static, (Phone, Settings, Option<String>)> {
        Harness::builder()
            .with_context_setup(ui::install)
            .build_ui_state(
                |ui, (phone, settings, dialed): &mut (Phone, Settings, Option<String>)| {
                    ui::dialog(ui.ctx(), "phone-dialer", "Phone", |ui| {
                        if phone.call.is_some() {
                            in_call(ui, phone);
                        } else if let Some(typed) = dialer(ui, phone, settings) {
                            *dialed = Some(typed);
                        }
                    });
                },
                (phone, settings, None),
            )
    }

    #[test]
    fn events_from_old_calls_cannot_end_or_reopen_a_new_call() {
        let (control, _received) = unbounded_channel();
        let a = phone::call::next_generation();
        let b = phone::call::next_generation();
        let mut phone = Phone {
            call: Some(Active {
                generation: b,
                label: "B".into(),
                status: Status::Calling,
                since: None,
                control,
            }),
            ..Default::default()
        };
        update(&mut phone, a, Event::Status(Status::Talking));
        update(&mut phone, a, Event::Ended(Some("late failure".into())));
        update(&mut phone, a, Event::Ended(None));
        assert_eq!(phone.call.as_ref().unwrap().generation, b);
        assert_eq!(phone.call.as_ref().unwrap().status, Status::Calling);
        assert!(!phone.open);
        assert!(phone.error.is_none());
        update(&mut phone, b, Event::Status(Status::Talking));
        assert_eq!(phone.call.as_ref().unwrap().status, Status::Talking);
        phone.hang_up();
        update(&mut phone, b, Event::Ended(Some("late failure".into())));
        assert!(!phone.open);
        assert!(phone.error.is_none());
    }

    #[test]
    fn contacts_and_the_keypad_dial() {
        let settings = Settings {
            phone_contacts: vec![
                Contact {
                    name: "Anna".into(),
                    number: "030 1234".into(),
                },
                Contact {
                    name: String::new(),
                    number: "0170 555".into(),
                },
            ],
            ..Settings::default()
        };
        let mut harness = harness(Phone::default(), settings);
        harness.run_steps(3);
        harness
            .get_by_role_and_label(egui::accesskit::Role::Button, "Anna")
            .click();
        harness.run_steps(2);
        assert_eq!(harness.state().2.as_deref(), Some("030 1234"));

        harness.state_mut().2 = None;
        harness.get_by_label(t!("phone.keypad")).click();
        harness.run_steps(2);
        harness.get_by_label("1").click();
        harness.run_steps(1);
        harness.get_by_label("2").click();
        harness.run_steps(1);
        harness.get_by_label(t!("phone.call")).click();
        harness.run_steps(2);
        assert_eq!(harness.state().2.as_deref(), Some("12"));
    }

    #[test]
    fn in_a_call_keys_send_tones_and_hang_up_ends_it() {
        let (control, mut received) = unbounded_channel();
        let phone = Phone {
            call: Some(Active {
                generation: phone::call::next_generation(),
                label: "📞 Anna".into(),
                status: Status::Talking,
                since: Some(Instant::now()),
                control,
            }),
            ..Phone::default()
        };
        let mut harness = harness(phone, Settings::default());
        harness.run_steps(3);
        harness.get_by_label("5").click();
        harness.run_steps(1);
        assert!(matches!(received.try_recv(), Ok(Control::Key('5'))));
        harness.get_by_label(t!("phone.hang_up")).click();
        harness.run_steps(1);
        assert!(matches!(received.try_recv(), Ok(Control::HangUp)));
        assert!(harness.state().0.call.is_none());
    }
}
