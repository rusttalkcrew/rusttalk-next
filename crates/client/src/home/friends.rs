//! The Friends view on the DM home, and doing friend things (#71).

use eframe::egui::{Align, Label, Layout, ScrollArea, Ui, Vec2};
use rusttalk_core::{client::FriendSignal, presence::Presence};

use super::{
    Home, Status, View, empty, face, pane_title,
    person::{self, Person},
};
use crate::{
    friends::{Act, Relation, now_ms},
    net::{Command, Draft},
    server::{Body, Directory, Id, Msg},
    ui::{self, Look},
};

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Tab {
    #[default]
    Online,
    All,
    Pending,
    Blocked,
}

const ROW: f32 = 52.0;
const BUTTON: Vec2 = Vec2::splat(ui::ICON_BUTTON);

impl Home {
    /// Shared servers that are online, where a friend message can go now.
    pub(super) fn reachable(&self, person: &Id) -> Vec<usize> {
        (self.shared(person).into_iter())
            .filter(|index| self.servers[*index].status == Status::Online)
            .collect()
    }

    /// Do something about someone, tell them when it concerns them, and sync it.
    pub(super) fn befriend(&mut self, person: Id, act: Act, now: f64) {
        let targets = self.reachable(&person);
        // Asking and saying yes mean nothing until they hear it; the rest works offline.
        if targets.is_empty() && matches!(act, Act::Add | Act::Accept) {
            self.toast = Some((t!("home.waiting_try_again").into(), now + 6.0));
            return;
        }
        let name = self.peer_name(&person);
        if let Some(signal) = self.friends.act(person, act, &name, now_ms()) {
            self.signal(person, signal, &targets);
        }
        self.publish_friends();
    }

    /// Send a friend message in the DM on each server, with one ID so the copies merge.
    fn signal(&self, person: Id, signal: FriendSignal, targets: &[usize]) {
        let Ok(id) = rusttalk_core::keys::random_key() else {
            tracing::warn!("no randomness for a message ID");
            return;
        };
        for index in targets {
            self.servers[*index].send(Command::Send(Draft {
                id: *id,
                channel: [0; 32],
                peer: Some(person),
                text: String::new(),
                reply_to: None,
                edit_of: None,
                revision: 0,
                files: Vec::new(),
                keep: Vec::new(),
                sticker: None,
                friend: Some((signal, now_ms().max(0) as u64)),
                poke: false,
                poll: None,
                voice_request: None,
                preview: None,
                inline: Vec::new(),
            }));
        }
    }

    /// A friend message in the DM with `peer`, live or loaded: update the list and
    /// answer it when needed. Ours were handled when sent.
    pub(super) fn friend_message(&mut self, peer: Id, msg: &Msg) {
        let Body::Friend(signal, sent) = msg.body else {
            return;
        };
        if msg.person != peer {
            return;
        }
        let name = self.peer_name(&peer);
        let answer = self
            .friends
            // Ordered by the signed send time, not the server's.
            .receive(
                peer,
                (msg.id, sent.min(i64::MAX as u64) as i64, signal),
                &name,
                now_ms(),
            );
        if let Some(answer) = answer {
            self.signal(peer, answer, &self.reachable(&peer));
        }
        self.publish_friends();
    }

    pub(super) fn publish_friends(&mut self) {
        let edits = self.friends.edits();
        if !edits.is_empty() {
            self.net.sync(edits);
        }
    }

    /// The person's presence over every shared server: online wins, then away.
    fn presence(&self, person: &Id) -> Presence {
        let seen = (self.servers.iter())
            .filter_map(|server| server.member(person))
            .map(|member| member.presence);
        seen.min_by_key(|presence| match presence {
            Presence::Online => 0,
            Presence::Away => 1,
            Presence::Offline => 2,
        })
        .unwrap_or(Presence::Offline)
    }

    /// Show the DM with them, making it on a shared server first when there is none.
    pub(super) fn open_dm(&mut self, person: Id, now: f64) {
        self.layout.list = false;
        if !self.replicas(&person).is_empty() {
            self.view = View::Direct(Some(person));
            return;
        }
        match self.reachable(&person).first() {
            Some(index) => self.servers[*index].send(Command::OpenDm(person)),
            None => self.toast = Some((t!("home.waiting_try_again").into(), now + 6.0)),
        }
    }

    /// Whether the person allows DMs from friends only.
    pub(super) fn friends_only(&self) -> bool {
        self.voice.audio.settings().friends_only
    }
}

/// The Friends page: tabs, and a row per person with what can be done.
pub fn view(ui: &mut Ui, home: &mut Home) {
    let now = ui.input(|input| input.time);
    pane_title(ui, "friends-title", t!("friends.title"), None, false, None);
    super::toast_line(ui, &mut home.toast);
    let incoming = (home.friends.contacts())
        .filter(|(_, c)| c.relation == Relation::Incoming)
        .count();
    let pending = if incoming > 0 {
        t!("friends.tab_pending_count", number = incoming)
    } else {
        t!("friends.tab_pending").into()
    };
    ui.add_space(ui::SPACE_S);
    ui::segmented(
        ui,
        &mut home.friends_tab,
        &[
            (Tab::Online, t!("status.online"), ""),
            (Tab::All, t!("friends.tab_all"), ""),
            (Tab::Pending, &pending, ""),
            (Tab::Blocked, t!("friends.tab_blocked"), ""),
        ],
    );
    ui.add_space(ui::SPACE_S);
    let tab = home.friends_tab;
    let people: Vec<(Id, Relation, String)> = (home.friends.contacts())
        .filter(|(person, contact)| match tab {
            Tab::Online => {
                contact.relation == Relation::Friend && home.presence(person) != Presence::Offline
            }
            Tab::All => contact.relation == Relation::Friend,
            Tab::Pending => {
                matches!(contact.relation, Relation::Incoming | Relation::Outgoing)
            }
            Tab::Blocked => contact.relation == Relation::Blocked,
        })
        .map(|(person, contact)| {
            let name = match home.dm_server(person) {
                Some(_) => home.peer_name(person),
                None if !contact.name.is_empty() => contact.name.clone(),
                None => home.peer_name(person),
            };
            (*person, contact.relation, name)
        })
        .collect();
    if people.is_empty() {
        let hint = match tab {
            Tab::Online => t!("friends.empty_online"),
            Tab::All => t!("friends.empty_all"),
            Tab::Pending => t!("friends.empty_pending"),
            Tab::Blocked => t!("friends.empty_blocked"),
        };
        empty(ui, t!("friends.title"), hint);
        return;
    }
    let mut todo = None;
    let mut card = None;
    let none = Directory::default();
    ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
        for (person, relation, name) in &people {
            let presence = home.presence(person);
            let caption = match relation {
                Relation::Incoming => t!("friends.wants"),
                Relation::Outgoing => t!("friends.sent"),
                Relation::Blocked => t!("friends.tab_blocked"),
                _ => match presence {
                    Presence::Online => t!("status.online"),
                    Presence::Away => t!("status.away"),
                    Presence::Offline => t!("friends.offline"),
                },
            };
            let dir = home
                .dm_server(person)
                .map_or(&none, |i| &home.servers[i].dir);
            ui::right_row(ui, ROW, |ui| {
                let button = |ui: &mut Ui, glyph: &str, tip: &str| {
                    ui::icon_button(ui, glyph, tip, Look::Plain, BUTTON).clicked()
                };
                match relation {
                    Relation::Friend => {
                        let more =
                            ui::icon_button(ui, "…", t!("friends.more"), Look::Plain, BUTTON);
                        ui::menu(&more, |m| {
                            if m.item("👋", t!("friends.remove")).clicked() {
                                todo = Some((*person, Some(Act::Remove)));
                            }
                            if m.danger("🚫", t!("friends.block")).clicked() {
                                todo = Some((*person, Some(Act::Block)));
                            }
                        });
                        if button(ui, "💬", t!("home.message")) {
                            todo = Some((*person, None));
                        }
                    }
                    Relation::Incoming => {
                        if button(ui, "✖", t!("friends.decline")) {
                            todo = Some((*person, Some(Act::Decline)));
                        }
                        if button(ui, "✔", t!("friends.accept")) {
                            todo = Some((*person, Some(Act::Accept)));
                        }
                    }
                    Relation::Outgoing => {
                        if button(ui, "✖", t!("friends.cancel_request")) {
                            todo = Some((*person, Some(Act::Cancel)));
                        }
                    }
                    Relation::Blocked => {
                        if ui::secondary(ui, t!("friends.unblock")).clicked() {
                            todo = Some((*person, Some(Act::Unblock)));
                        }
                    }
                    Relation::None => {}
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let row = ui::row_button(ui, ROW - 4.0, name, |ui| {
                        face(ui, dir, person, name, 34.0, *relation == Relation::Friend);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.add(Label::new(ui::bold(name)).truncate());
                            ui.add(Label::new(ui::caption(caption)).truncate());
                        });
                    });
                    // Their profile card, as on a server you share.
                    if let Some(index) = home
                        .dm_server(person)
                        .filter(|i| home.servers[*i].member(person).is_some())
                    {
                        let target = Person::of(home, &home.servers[index], *person);
                        if let Some(pick) = person::interact(ui, &row, &target, false) {
                            card = Some((index, *person, pick));
                        }
                    }
                });
            });
            ui.separator();
        }
    });
    if let Some((index, person, pick)) = card {
        person::apply(home, index, person, pick, now);
    }
    match todo {
        Some((person, Some(act))) => home.befriend(person, act, now),
        Some((person, None)) => home.open_dm(person, now),
        None => {}
    }
}

/// The card above a DM with someone who isn't a friend yet, or who asked to be one.
/// Returns what the person picked.
pub fn dm_card(ui: &mut Ui, relation: Relation, request: bool, name: &str) -> Option<Act> {
    let (text, yes, no) = match relation {
        Relation::Incoming => (
            t!("friends.card_request", name = name),
            (t!("friends.accept"), Act::Accept),
            (t!("friends.decline"), Act::Decline),
        ),
        _ if request => (
            t!("friends.card_stranger", name = name),
            (t!("friends.add"), Act::Add),
            (t!("friends.block"), Act::Block),
        ),
        _ => return None,
    };
    let mut picked = None;
    ui.add_space(ui::SPACE_S);
    ui::card(ui, |ui| {
        ui.add(Label::new(text).wrap());
        ui.add_space(ui::SPACE_S);
        ui.horizontal(|ui| {
            if ui::primary(ui, yes.0).clicked() {
                picked = Some(yes.1);
            }
            if ui::secondary(ui, no.0).clicked() {
                picked = Some(no.1);
            }
        });
    });
    ui.add_space(ui::SPACE_S);
    picked
}

/// The friend items of a person's menu, after a separator.
pub fn menu_items(m: &mut ui::Menu, relation: Relation) -> Option<Act> {
    m.separator();
    let (icon, label, act) = match relation {
        Relation::None => ("➕", t!("friends.add"), Act::Add),
        Relation::Incoming => ("✔", t!("friends.accept_request"), Act::Accept),
        Relation::Outgoing => ("✖", t!("friends.cancel_friend_request"), Act::Cancel),
        Relation::Friend => ("👋", t!("friends.remove"), Act::Remove),
        Relation::Blocked => ("🚫", t!("friends.unblock"), Act::Unblock),
    };
    let mut picked = m.item(icon, label).clicked().then_some(act);
    if relation != Relation::Blocked && m.danger("🚫", t!("friends.block")).clicked() {
        picked = Some(Act::Block);
    }
    picked
}

/// Whether a friend message shows in the chat; answers nobody needs to see don't.
pub fn shown(signal: FriendSignal) -> bool {
    matches!(signal, FriendSignal::Request | FriendSignal::Accept)
}

/// The line a shown friend message has in the chat.
pub fn line(signal: FriendSignal, name: &str) -> String {
    match signal {
        FriendSignal::Accept => t!("friends.line_accept", name = name),
        _ => t!("friends.line_request", name = name),
    }
}
