//! The Friends view on the DM home, and doing friend things (#71).

use eframe::egui::{Align, Label, Layout, ScrollArea, Ui, Vec2};
use rusttalk_core::{client::FriendSignal, presence::Presence};

use super::{
    Home, Status, View, empty, face, pane_title,
    person::{self, Person},
};
use crate::{
    friends::{Act, Relation, Sort, now_ms},
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
        let was_blocked = self.friends.is_blocked(&person);
        if let Some(signal) = self.friends.act(person, act, &name, now_ms()) {
            self.signal(person, signal, &targets);
        }
        if self.friends.is_blocked(&person) != was_blocked {
            self.mute_voice(person, !was_blocked);
        }
        self.publish_friends();
    }

    /// Block someone: they look unfriended, and their DMs, pokes, friend requests and
    /// voice requests are dropped on this person's devices. Synced, never sent to a server.
    pub fn block(&mut self, person: Id, now: f64) {
        self.befriend(person, Act::Block, now);
    }

    pub fn unblock(&mut self, person: Id, now: f64) {
        self.befriend(person, Act::Unblock, now);
    }

    pub fn is_blocked(&self, person: &Id) -> bool {
        self.friends.is_blocked(person)
    }

    /// Their voice in calls, on this device only, as the call mixer sets it.
    fn mute_voice(&self, person: Id, muted: bool) {
        let audio = &self.voice.audio;
        let mut all = audio.mix();
        let mut mix = all.person(&person);
        mix.voice.muted = muted;
        all.set_person(&person, mix);
        audio.set_mix(all);
    }

    /// Send a friend message in the DM on each server, with one ID so the copies merge.
    fn signal(&self, person: Id, signal: FriendSignal, targets: &[usize]) {
        let Ok(id) = rusttalk_core::keys::random_key() else {
            tracing::warn!("no randomness for a message ID");
            return;
        };
        for index in targets {
            self.servers[*index].send(Command::Send(Draft {
                forum_post: false,
                queued: Default::default(),
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

    /// The person's presence over every shared server: online wins, then do not
    /// disturb, then away.
    fn presence(&self, person: &Id) -> Presence {
        let seen = (self.servers.iter())
            .filter_map(|server| server.member(person))
            .map(|member| member.presence);
        seen.min_by_key(|presence| match presence {
            Presence::Online => 0,
            Presence::Dnd => 1,
            Presence::Away => 2,
            Presence::Offline => 3,
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

    /// Where the DM with them goes, by who this person allows to message them.
    pub(super) fn dm_sort(&self, peer: &Id) -> Sort {
        let allow = self.voice.audio.snapshot().allow_dms;
        self.friends.sort(peer, allow, self.newest_from(peer))
    }

    /// Everyone with a DM, split into the normal list and message requests.
    pub(super) fn dm_split(&self) -> (Vec<Id>, Vec<Id>) {
        let sorted = self
            .dm_peers()
            .into_iter()
            .map(|peer| (peer, self.dm_sort(&peer)));
        let (mut shown, mut requests) = (Vec::new(), Vec::new());
        for (peer, sort) in sorted {
            match sort {
                Sort::Shown => shown.push(peer),
                Sort::Request => requests.push(peer),
                Sort::Hidden => {}
            }
        }
        (shown, requests)
    }

    /// The server time of their newest message loaded in the DM.
    fn newest_from(&self, peer: &Id) -> Option<i64> {
        let thread = self.threads.get(peer)?;
        (thread.chat.messages.iter())
            .filter(|msg| msg.person == *peer && msg.body != Body::Unreadable)
            .map(|msg| msg.at)
            .max()
    }

    /// Accept or delete their message request, here and on the other devices.
    pub(super) fn answer_request(&mut self, peer: Id, accept: bool) {
        let name = self.peer_name(&peer);
        if accept {
            self.friends.accept(peer, &name, now_ms());
        } else {
            let newest = self.newest_from(&peer);
            self.friends.dismiss(peer, newest, &name, now_ms());
            self.view = View::Direct(None);
        }
        self.publish_friends();
    }

    /// Writing to someone lets their DMs in, like accepting their request; also for
    /// conversations from before message requests existed.
    pub(super) fn wrote_to(&mut self, peer: Id, msg: &Msg) {
        let me = *self.identity.certificate.person_id();
        if msg.person != me
            || !matches!(msg.body, Body::Text(_) | Body::Poll(..))
            || !matches!(
                self.friends.relation(&peer),
                Relation::None | Relation::Incoming
            )
        {
            return;
        }
        let name = self.peer_name(&peer);
        if self.friends.accept(peer, &name, now_ms()) {
            self.publish_friends();
        }
    }
}

/// The Friends page: tabs, and a row per person with what can be done.
pub fn view(ui: &mut Ui, home: &mut Home) {
    let now = ui.input(|input| input.time);
    pane_title(ui, "friends-title", t!("friends.title"), None, None);
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
                None if !contact.name.is_empty()
                    && !rusttalk_core::names::contains_link(&contact.name) =>
                {
                    contact.name.clone()
                }
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
                    Presence::Dnd => t!("status.dnd"),
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

/// What the card on top of a DM asks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Card {
    Friend(Act),
    /// Accept or delete a message request.
    Request(bool),
}

/// The card above a DM with someone who asked to be friends, or a message request.
/// Returns what the person picked.
pub fn dm_card(ui: &mut Ui, relation: Relation, request: bool, name: &str) -> Option<Card> {
    let (text, buttons) = match relation {
        Relation::Incoming => (
            t!("friends.card_request", name = name),
            vec![
                (t!("friends.accept"), Card::Friend(Act::Accept)),
                (t!("friends.decline"), Card::Friend(Act::Decline)),
            ],
        ),
        _ if request => (
            t!("friends.card_stranger", name = name),
            vec![
                (t!("friends.accept"), Card::Request(true)),
                (t!("friends.delete_request"), Card::Request(false)),
                // Blocking as #667 grows it; `Act::Block` is the one place it starts.
                (t!("friends.block"), Card::Friend(Act::Block)),
            ],
        ),
        _ => return None,
    };
    let mut picked = None;
    ui.add_space(ui::SPACE_S);
    ui::card(ui, |ui| {
        ui.add(Label::new(text).wrap());
        ui.add_space(ui::SPACE_S);
        ui.horizontal_wrapped(|ui| {
            for (index, (label, card)) in buttons.into_iter().enumerate() {
                let button = match card {
                    _ if index == 0 => ui::primary(ui, label),
                    Card::Friend(Act::Block) => ui::danger(ui, label),
                    _ => ui::secondary(ui, label),
                };
                if button.clicked() {
                    picked = Some(card);
                }
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
