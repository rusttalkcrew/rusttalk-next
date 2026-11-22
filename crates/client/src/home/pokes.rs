//! Pokes (#112): sending one, the pop-up when one comes, and the Pokes page.

use eframe::egui::{self, Align, Label, Layout, ScrollArea, Ui, Vec2};
use rusttalk_core::client::MAX_POKE_CHARS;

use super::{Dialog, Home, View, empty, face, pane_title, time};
use crate::{
    friends::Relation,
    net::{Command, Draft},
    notify::{self, Arrival, Mention},
    pokes::Poke,
    server::{Directory, Id, Msg},
    ui::{self, Look},
};

const ROW: f32 = 56.0;
const BUTTON: Vec2 = Vec2::splat(ui::ICON_BUTTON);
/// How long the pop-up stays, in seconds.
const SHOWN: f64 = 10.0;

/// The pop-up for the newest poke: who, their note, and until when it shows.
pub struct Popup {
    person: Id,
    name: String,
    text: String,
    until: f64,
}

impl Home {
    /// Send a poke as a DM message to every shared server that is online, with one
    /// ID so the copies merge, and keep it in the history.
    pub(super) fn poke(&mut self, person: Id, text: String, now: f64) {
        let targets = self.reachable(&person);
        let (Some(first), Ok(id)) = (targets.first(), rusttalk_core::keys::random_key()) else {
            self.toast = Some((t!("home.waiting_try_again").into(), now + 6.0));
            return;
        };
        let server = self.servers[*first].name().to_owned();
        for index in &targets {
            self.servers[*index].send(Command::Send(Draft {
                queued: Default::default(),
                id: *id,
                channel: [0; 32],
                peer: Some(person),
                text: text.clone(),
                reply_to: None,
                edit_of: None,
                revision: 0,
                files: Vec::new(),
                keep: Vec::new(),
                sticker: None,
                friend: None,
                poke: true,
                poll: None,
                preview: None,
                inline: Vec::new(),
            }));
        }
        let at = notify::unix_now();
        let poke = Poke {
            id: *id,
            person,
            incoming: false,
            text,
            at,
            server,
            read: true,
        };
        self.pokes.add(poke, true, at);
    }

    /// A poke in the DM with `peer` on server `index`: keep it, and for a live one
    /// from them, pop up and notify as the DM's notification settings allow.
    /// Blocked people's pokes, and with "friends only" strangers', are dropped.
    pub(super) fn poke_arrived(
        &mut self,
        ctx: &egui::Context,
        (index, channel): (usize, Id),
        peer: Id,
        msg: &Msg,
        live: bool,
    ) {
        if live && msg.backfilled {
            return;
        }
        let crate::server::Body::Poke(text) = &msg.body else {
            return;
        };
        let me = *self.identity.certificate.person_id();
        let incoming = msg.person == peer;
        if !incoming && msg.person != me {
            return;
        }
        if incoming
            && (self.friends.relation(&peer) == Relation::Blocked
                || self.friends.is_request(&peer, self.friends_only()))
        {
            return;
        }
        let server = &self.servers[index];
        let poke = Poke {
            id: msg.id,
            person: peer,
            incoming,
            text: text.clone(),
            at: msg.at,
            server: server.name().to_owned(),
            read: msg.backfilled,
        };
        let now = notify::unix_now();
        if !self.pokes.add(poke, live, now) || !live || !incoming {
            return;
        }
        let prefs = self.notify.prefs();
        let rule = prefs.effective(&server.address, &channel, true, now);
        if rule.muted || rule.level == notify::Level::Nothing {
            return;
        }
        let name = server.member_name(&peer);
        // A poke calls on this person, like a mention.
        let arrival = Arrival {
            dm: true,
            mention: Mention::Me,
        };
        if notify::should_notify(prefs, rule, arrival, self.focused, false) {
            let body = match text.trim() {
                "" => t!("pokes.no_note"),
                note => note,
            };
            let title = t!("pokes.poked_you", name = name);
            let target = (server.address.clone(), channel);
            self.notify.show(ctx, &title, body, target);
        }
        self.popup = Some(Popup {
            person: peer,
            name,
            text: text.clone(),
            until: ctx.input(|input| input.time) + SHOWN,
        });
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(SHOWN));
    }
}

/// The pop-up at the top right for the newest poke, with poke back and message.
pub fn popup(ctx: &egui::Context, home: &mut Home) {
    let now = ctx.input(|input| input.time);
    let Some(popup) = home.popup.as_ref().filter(|popup| popup.until > now) else {
        home.popup = None;
        return;
    };
    let (person, mut close, mut act) = (popup.person, false, None);
    ui::toast(ctx, "poke", |ui| {
        ui.horizontal(|ui| {
            let title = format!("👉 {}", t!("pokes.poked_you", name = popup.name));
            ui.add(Label::new(ui::bold(title)).wrap());
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                close = ui::small_close(ui, t!("common.dismiss")).clicked();
            });
        });
        if !popup.text.trim().is_empty() {
            ui.add(Label::new(crate::emoji::text(&popup.text)).wrap());
        }
        ui.add_space(ui::SPACE_S);
        ui.horizontal(|ui| {
            if ui::primary(ui, t!("pokes.poke_back")).clicked() {
                act = Some(true);
            }
            if ui::secondary(ui, t!("home.message")).clicked() {
                act = Some(false);
            }
        });
    });
    match act {
        Some(true) => home.dialog = Some(Dialog::Poke(person, String::new())),
        Some(false) => home.open_dm(person, now),
        None => {}
    }
    if close || act.is_some() {
        home.popup = None;
    }
}

/// The poke dialog's content: a short note and the Poke button. Returns the note
/// once sent.
pub fn dialog(ui: &mut Ui, note: &mut String, close: &mut bool) -> Option<String> {
    let field = ui.add(
        ui::field(note)
            .hint_text(t!("pokes.note_hint"))
            .char_limit(MAX_POKE_CHARS)
            .desired_width(f32::INFINITY),
    );
    if ui.memory(|memory| memory.focused().is_none()) {
        field.request_focus();
    }
    let mut send = field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
    ui::footer(ui, |ui| {
        send |= ui::primary(ui, t!("pokes.poke")).clicked();
        *close |= ui::secondary(ui, t!("common.cancel")).clicked();
    });
    send.then(|| note.trim().to_owned())
}

/// The Pokes page: received and sent, newest first.
pub fn view(ui: &mut Ui, home: &mut Home) {
    let now = ui.input(|input| input.time);
    pane_title(ui, "pokes-title", t!("pokes.title"), None, false, None);
    super::toast_line(ui, &mut home.toast);
    if home.focused {
        home.pokes.mark_read();
    }
    if home.pokes.list().next().is_none() {
        empty(ui, t!("pokes.title"), t!("pokes.empty"));
        return;
    }
    let none = Directory::default();
    let mut todo = None;
    ui.add_space(ui::SPACE_S);
    ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        for poke in home.pokes.list() {
            let name = home.peer_name(&poke.person);
            let dir = (home.dm_server(&poke.person)).map_or(&none, |i| &home.servers[i].dir);
            let reachable = !home.reachable(&poke.person).is_empty();
            ui::right_row(ui, ROW, |ui| {
                let button = |ui: &mut Ui, glyph: &str, tip: &str| {
                    ui::icon_button(ui, glyph, tip, Look::Plain, BUTTON).clicked()
                };
                if button(ui, "💬", t!("home.message")) {
                    todo = Some((poke.person, false));
                }
                let label = if poke.incoming {
                    t!("pokes.poke_back")
                } else {
                    t!("pokes.poke_again")
                };
                let back = ui.add_enabled_ui(reachable, |ui| button(ui, "👉", label));
                if back.inner {
                    todo = Some((poke.person, true));
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    face(ui, dir, &poke.person, &name, 34.0, false);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let title = match poke.incoming {
                            true => t!("pokes.poked_you", name = name),
                            false => t!("pokes.you_poked", name = name),
                        };
                        ui.add(Label::new(ui::bold(title)).truncate());
                        if !poke.text.is_empty() {
                            ui.add(Label::new(crate::emoji::text(&poke.text)).truncate());
                        }
                        let caption = format!("{} · {}", time(poke.at), poke.server);
                        ui.add(Label::new(ui::caption(caption)).truncate());
                    });
                });
            });
            ui.separator();
        }
    });
    match todo {
        Some((person, true)) => home.dialog = Some(Dialog::Poke(person, String::new())),
        Some((person, false)) => home.open_dm(person, now),
        None => {}
    }
}

/// The Pokes row of the DM list, with the unread count.
pub fn nav_row(ui: &mut Ui, home: &mut Home) {
    let label = t!("pokes.title");
    let open = home.view == View::Pokes;
    let row = ui::nav_row(ui, "👉", label, open, egui::accesskit::Role::Button, label);
    let unread = home.pokes.unread();
    if unread > 0 {
        let at = egui::pos2(row.rect.right() - 16.0, row.rect.center().y);
        ui::mention_badge(ui, at, unread as u32, ui::colors(ui).side);
    }
    if row.clicked() {
        home.view = View::Pokes;
        home.layout.list = false;
    }
}
