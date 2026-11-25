//! Someone in the member list, the voice list under a channel or a call tile. A left
//! click shows their profile card and a right click the one menu with everything
//! that can be done with them there, so they behave the same everywhere.

use eframe::egui::{CursorIcon, Label, Response, Role, Sense, Ui, WidgetInfo};
use rusttalk_core::presence::Presence;

use super::{Dialog, Home, Tab, member_face};
use crate::{
    admin::{self, Moderate},
    audio::Audio,
    call,
    friends::{Act, Friends, Relation},
    net::{Admin, Command},
    server::{Id, Member, Server},
    ui,
};

/// The owner's role rank on the server; nobody outranks it.
const OWNER_RANK: i64 = 1000;

/// What was picked on someone's card or in their menu.
pub enum Pick {
    Message,
    Poke,
    /// Ask them into the voice channel you are in.
    AskToJoin,
    Details,
    EditProfile,
    Befriend(Act),
    ShowVideo(bool),
    /// Mute for everyone, move or remove from a voice channel.
    Voice(Admin),
    Moderate(Moderate),
}

/// One person on one server, with what the place they were clicked in adds.
pub struct Person<'a> {
    pub server: &'a Server,
    pub person: Id,
    pub relation: Relation,
    /// You are in a voice channel here and they aren't in it.
    pub can_ask: bool,
    /// Set while you are in a call with them: their volume and local mute.
    pub audio: Option<&'a Audio>,
    /// Their camera is on, and whether you show it; call tiles only.
    pub video: Option<bool>,
}

impl<'a> Person<'a> {
    /// `person` as seen from anywhere on `server`.
    pub fn of(home: &'a Home, server: &'a Server, person: Id) -> Self {
        Self::with(&home.friends, &home.voice, server, person)
    }

    /// `of` from the parts of `Home` it reads, for code that holds another part of
    /// `Home` mutably, like the server settings window.
    pub fn with(
        friends: &'a Friends,
        voice: &'a call::Voice,
        server: &'a Server,
        person: Id,
    ) -> Self {
        let listening =
            voice_channel(server, &person).is_some_and(|channel| voice.in_call(server, &channel));
        Person {
            server,
            person,
            relation: friends.relation(&person),
            can_ask: !listening && super::voice_requests::calling(voice, server),
            audio: listening.then_some(&voice.audio),
            video: None,
        }
    }

    fn me(&self) -> bool {
        self.person == self.server.dir.me
    }

    fn member(&self) -> Option<&'a Member> {
        self.server.member(&self.person)
    }

    /// The menu: Message and Poke, details, friends, volume in a call, then what a
    /// moderator may do. Only what applies to this person here is shown.
    pub fn menu(&self, m: &mut ui::Menu) -> Option<Pick> {
        let mut pick = None;
        let other = !self.me();
        let known = self.member().is_some();
        if other && known {
            pick = pick.or(self.talk_items(m));
        }
        if known && m.item("ℹ", t!("common.details")).clicked() {
            pick = pick.or(Some(Pick::Details));
        }
        if !other && m.item("✏", t!("settings.edit_profile")).clicked() {
            pick = pick.or(Some(Pick::EditProfile));
        }
        if other && known {
            pick = pick.or(super::friends::menu_items(m, self.relation).map(Pick::Befriend));
        }
        if let (true, Some(audio)) = (other, self.audio) {
            m.separator();
            let apps = (self.server.voice.values().flatten())
                .find(|peer| peer.person == self.person)
                .map_or(&[][..], |peer| &peer.apps);
            call::mixer::show(m, audio, self.person, true, apps, |sid, on| {
                self.server.send(Command::Listen(sid, on))
            });
            if let Some(shown) = self.video {
                let item = ui::Item::new("📷", t!("call.show_video")).checked(shown);
                if m.add(item).clicked() {
                    pick = pick.or(Some(Pick::ShowVideo(!shown)));
                }
            }
        }
        if !other || !self.outranked() {
            return pick;
        }
        let perms = self.server.dir.perms;
        if let Some(channel) = voice_channel(self.server, &self.person)
            && (perms.mute_members || perms.move_members)
        {
            m.separator();
            pick = pick
                .or(call::moderation_menu(m, self.server, &channel, self.person).map(Pick::Voice));
        }
        if perms.kick || perms.ban || perms.manage_roles {
            m.separator();
            let mut choice = None;
            admin::member_menu(m, &mut choice, perms);
            pick = pick.or(choice.map(Pick::Moderate));
        }
        pick
    }

    /// Whether you may moderate them, as the server decides it (`Authority::outrank`):
    /// their highest role ranks strictly below yours and below the owner's, which
    /// nobody outranks. The admin role counts by its rank only.
    pub(crate) fn outranked(&self) -> bool {
        let Some(member) = self.member() else {
            return false;
        };
        let dir = &self.server.dir;
        let rank = (dir.roles.iter())
            .filter(|role| member.roles.contains(&role.id))
            .map(|role| role.rank)
            .max()
            .unwrap_or(-1);
        !member.roles.iter().any(|role| role == "owner")
            && rank < OWNER_RANK
            && rank < dir.perms.rank
    }

    fn talk_items(&self, m: &mut ui::Menu) -> Option<Pick> {
        let mut pick = None;
        if m.item("💬", t!("home.message")).clicked() {
            pick = Some(Pick::Message);
        }
        if self.relation != Relation::Blocked && m.item("👉", t!("pokes.poke")).clicked() {
            pick = Some(Pick::Poke);
        }
        if self.relation != Relation::Blocked
            && self.can_ask
            && m.item("📞", t!("voice_request.ask")).clicked()
        {
            pick = Some(Pick::AskToJoin);
        }
        pick
    }

    /// The profile card: picture, name, status, roles, bio, and Message and Poke (or
    /// Edit profile on your own).
    pub fn card(&self, ui: &mut Ui) -> Option<Pick> {
        let dir = &self.server.dir;
        let member = self.member();
        let name = self.server.member_name(&self.person);
        let presence = member.map_or(Presence::Offline, |m| m.presence);
        ui.horizontal(|ui| {
            member_face(ui, member, &self.person, &name, 56.0, true);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let roles = member.map_or(&[][..], |m| &m.roles);
                let mut text = ui::bold(&name).size(ui::TEXT_TITLE);
                if let Some(color) = dir.name_color(roles) {
                    text = text.color(color);
                }
                ui.add(Label::new(text).truncate());
                ui.add(Label::new(ui::caption(status_text(presence))).truncate());
            });
        });
        let roles: Vec<_> = (dir.roles.iter())
            .filter(|role| member.is_some_and(|m| m.roles.contains(&role.id)))
            .collect();
        if !roles.is_empty() {
            ui.add_space(ui::SPACE_S);
            ui::caption_label(ui, t!("admin.roles"));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = eframe::egui::vec2(ui::SPACE_S, ui::SPACE_XS);
                let c = ui::colors(ui);
                for role in roles {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = ui::SPACE_XS;
                        ui::dot(ui, role.color.unwrap_or(c.muted));
                        ui.label(ui::caption(&role.name).color(c.text));
                    });
                }
            });
        }
        if let Some(bio) = member
            .and_then(|m| m.bio.as_deref())
            .filter(|b| !b.is_empty())
        {
            ui.add_space(ui::SPACE_S);
            ui::caption_label(ui, t!("person.about"));
            super::markdown::bio(ui, dir, bio);
        }
        ui.add_space(ui::SPACE_M);
        let mut pick = None;
        ui.horizontal(|ui| {
            if self.me() {
                if ui::secondary(ui, t!("settings.edit_profile")).clicked() {
                    pick = Some(Pick::EditProfile);
                }
            } else if member.is_some() {
                if ui::primary(ui, t!("home.message")).clicked() {
                    pick = Some(Pick::Message);
                }
                if self.relation != Relation::Blocked
                    && ui::secondary(ui, t!("pokes.poke")).clicked()
                {
                    pick = Some(Pick::Poke);
                }
            }
        });
        // On a line of its own: with Message and Poke it doesn't fit the card.
        if !self.me() && member.is_some() && self.relation != Relation::Blocked && self.can_ask {
            ui.add_space(ui::SPACE_S);
            if ui::secondary(ui, t!("voice_request.ask")).clicked() {
                pick = Some(Pick::AskToJoin);
            }
        }
        if pick.is_some() {
            ui.close();
        }
        pick
    }
}

pub fn status_text(presence: Presence) -> &'static str {
    match presence {
        Presence::Online => t!("status.online"),
        Presence::Away => t!("status.away"),
        Presence::Dnd => t!("status.dnd"),
        Presence::Offline => t!("friends.offline"),
    }
}

/// The voice channel on `server` that `person` is in, if any.
fn voice_channel(server: &Server, person: &Id) -> Option<Id> {
    (server.voice.iter())
        .find(|(_, peers)| peers.iter().any(|peer| peer.person == *person))
        .map(|(channel, _)| *channel)
}

/// The card on a left click on `response` (opening where it was clicked with
/// `at_click`, for big tiles) and the menu on a right click.
pub fn interact(ui: &Ui, response: &Response, person: &Person, at_click: bool) -> Option<Pick> {
    let card = profile(ui, response, person, at_click);
    let menu = ui::context_menu_with_id(response, popup_id(response, person), |m| person.menu(m))
        .flatten();
    card.or(menu)
}

/// The shared profile card without a person context menu, for rows with their own menu.
pub(super) fn profile(
    ui: &Ui,
    response: &Response,
    person: &Person,
    at_click: bool,
) -> Option<Pick> {
    let name = person.server.member_name(&person.person);
    ui::card_popup(
        ui,
        popup_id(response, person),
        response,
        &name,
        at_click,
        |ui| person.card(ui),
    )
    .flatten()
}

pub(super) fn profile_trigger(ui: &Ui, response: Response, name: &str) -> Response {
    response.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), name));
    ui::focus_ring(ui, &response, ui::RADIUS_S);
    response.on_hover_cursor(CursorIcon::PointingHand)
}

pub(super) fn profile_name(ui: &mut Ui, name: &str) -> Response {
    let response = ui.add(
        Label::new(ui::bold(name))
            .truncate()
            .selectable(false)
            .sense(Sense::click()),
    );
    if response.hovered() {
        ui.painter().hline(
            response.rect.x_range(),
            response.rect.bottom(),
            (1.0, ui::colors(ui).text),
        );
    }
    profile_trigger(ui, response, name)
}

/// A popup must never inherit a different person when a positional row is reused.
pub(crate) fn popup_id(response: &Response, person: &Person) -> eframe::egui::Id {
    response
        .id
        .with((person.server.known.server_id, person.person))
}

/// Do what was picked for `person` on server `index`.
pub fn apply(home: &mut Home, index: usize, person: Id, pick: Pick, now: f64) {
    match pick {
        Pick::Message => home.open_dm(person, now),
        Pick::Poke => home.dialog = Some(Dialog::Poke(person, String::new())),
        Pick::AskToJoin => home.ask_to_join(index, person, now),
        Pick::Details => home.dialog = Some(Dialog::Member(index, person)),
        Pick::EditProfile => home.settings = Some(Tab::Profile),
        Pick::Befriend(act) => home.befriend(person, act, now),
        Pick::ShowVideo(true) => {
            home.voice.hidden.remove(&person);
        }
        Pick::ShowVideo(false) => {
            home.voice.hidden.insert(person);
        }
        Pick::Voice(request) => home.servers[index].send(Command::Admin(request)),
        Pick::Moderate(choice) => admin::moderate(home, index, person, choice),
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Sense, Vec2};
    use egui_kittest::{Harness, kittest::Queryable};
    use rusttalk_core::client::voice::VoiceStatus;

    use super::*;
    use crate::server::{Peer, Perms, Role, Status};

    const ME: Id = [1; 32];
    const ANN: Id = [7; 32];
    const LOUNGE: Id = [2; 32];

    struct Case {
        perms: Perms,
        person: Id,
        /// Ann is in the Lounge.
        in_voice: bool,
        /// You are in that call too.
        in_call: bool,
        ann_roles: &'static [&'static str],
    }

    fn member(person: Id, name: &str) -> Member {
        Member {
            person,
            name: Some(name.into()),
            devices: Vec::new(),
            roles: Vec::new(),
            avatar: None,
            presence: Presence::Online,
            hide_offline: false,
            bio: None,
            voice_muted: false,
        }
    }

    /// Right-click "Ann" (or yourself) in a window holding the shared menu, and
    /// return the window to look at the items.
    fn menu(case: Case) -> Harness<'static, Option<Home>> {
        let mut harness = Harness::builder()
            .with_size(Vec2::new(400.0, 700.0))
            .build_ui_state(
                move |ui, home: &mut Option<Home>| {
                    let Some(home) = home else { return };
                    let server = &home.servers[0];
                    let target = Person {
                        server,
                        person: case.person,
                        relation: Relation::None,
                        can_ask: false,
                        audio: case.in_call.then_some(&home.voice.audio),
                        video: None,
                    };
                    let (_, row) = ui.allocate_exact_size(Vec2::new(200.0, 30.0), Sense::click());
                    row.widget_info(|| {
                        eframe::egui::WidgetInfo::labeled(
                            eframe::egui::accesskit::Role::Button,
                            true,
                            "target",
                        )
                    });
                    ui::context_menu(&row, |m| target.menu(m));
                },
                None,
            );
        ui::install(&harness.ctx);
        let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Desktop").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut home = Home::new(identity, &harness.ctx, data.path().to_owned()).unwrap();
        let (commands, _) = tokio::sync::mpsc::unbounded_channel();
        let known = rusttalk_core::auth::KnownServer {
            address: "https://friends.example.org/".parse().unwrap(),
            server_id: [2; 32],
            tls_fingerprint: None,
        };
        let mut server = super::super::new_server(known, "Friends", commands);
        server.status = Status::Online;
        server.dir.me = ME;
        server.dir.perms = case.perms;
        let role = |id: &str, rank| Role {
            id: id.into(),
            name: id.into(),
            name_known: true,
            rank,
            permissions: Vec::new(),
            custom: false,
            color: None,
            separate: false,
        };
        server.dir.roles = vec![role("owner", 1000), role("admin", 900), role("mod", 500)];
        let mut ann = member(ANN, "Ann");
        ann.roles = case.ann_roles.iter().map(|id| id.to_string()).collect();
        server.dir.members = vec![member(ME, "Me"), ann].into();
        if case.in_voice {
            let peer = Peer {
                person: ANN,
                connecting: false,
                unknown: false,
                status: VoiceStatus::default(),
                apps: Vec::new(),
                screens: Vec::new(),
                camera: None,
                terminal: None,
                diagram: None,
            };
            server.voice.insert(LOUNGE, vec![peer]);
        }
        home.servers.push(server);
        *harness.state_mut() = Some(home);
        harness.run_steps(4);
        harness.get_by_label("target").click_secondary();
        harness.run_steps(4);
        harness
    }

    const BOB: Id = [8; 32];

    fn kick_harness(
        settings: bool,
    ) -> (
        Harness<'static, Option<Home>>,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
    ) {
        let mut fixture = menu(Case {
            perms: Perms {
                rank: 500,
                ..moderator()
            },
            person: ANN,
            in_voice: false,
            in_call: false,
            ann_roles: &[],
        });
        let mut home = fixture.state_mut().take().unwrap();
        home.servers[0].dir.members.edit().push(member(BOB, "Bob"));
        let (commands, sent) = tokio::sync::mpsc::unbounded_channel();
        home.servers[0].commands = commands;
        let (commands, other_sent) = tokio::sync::mpsc::unbounded_channel();
        let mut known = home.servers[0].known.clone();
        known.server_id = [9; 32];
        known.address = "https://other.example.org/".parse().unwrap();
        let mut other = super::super::new_server(known, "Other", commands);
        other.status = Status::Online;
        home.servers.push(other);
        if settings {
            admin::open(&mut home, 0, admin::Tab::Members);
        }
        let mut harness = Harness::builder()
            .with_size(Vec2::new(900.0, 800.0))
            .build_ui_state(
                |ui, home: &mut Option<Home>| {
                    let Some(home) = home else { return };
                    if home.server_settings.is_none() {
                        let server = &home.servers[0];
                        let mut picked = None;
                        for member in &server.dir.members {
                            let row = ui.button(member.display_name());
                            let target = Person::of(home, server, member.person);
                            if let Some(pick) = interact(ui, &row, &target, false) {
                                picked = Some((member.person, pick));
                            }
                        }
                        if let Some((person, pick)) = picked {
                            apply(home, 0, person, pick, 0.0);
                        }
                    }
                    admin::window(ui.ctx(), home);
                    admin::kick_dialog(ui.ctx(), home);
                },
                None,
            );
        ui::install(&harness.ctx);
        *harness.state_mut() = Some(home);
        harness.run_steps(4);
        (harness, sent, other_sent)
    }

    fn choose_kick(harness: &mut Harness<'static, Option<Home>>, name: &str) {
        harness
            .get_by_role_and_label(eframe::egui::accesskit::Role::Button, name)
            .click_secondary();
        harness.run_steps(4);
        harness.get_by_label(t!("admin.kick")).click();
        harness.run_steps(4);
    }

    fn drain_kicks(sent: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<Id> {
        let mut kicks = Vec::new();
        while let Ok(command) = sent.try_recv() {
            if let Command::Admin(Admin::Kick(person)) = command {
                kicks.push(person);
            }
        }
        kicks
    }

    #[test]
    fn kick_confirmation_keeps_target_and_sends_once_including_inside_settings() {
        for settings in [false, true] {
            let (mut harness, mut sent, mut other) = kick_harness(settings);
            choose_kick(&mut harness, "Ann");
            assert!(
                harness
                    .query_by_label(&t!("admin.kick_title", name = "Ann"))
                    .is_some()
            );
            assert!(drain_kicks(&mut sent).is_empty());
            assert_eq!(
                harness.state().as_ref().unwrap().server_settings.is_some(),
                settings
            );
            harness
                .get_by_role_and_label(eframe::egui::accesskit::Role::Button, "Bob")
                .click_secondary();
            harness.run_steps(4);
            assert!(
                harness
                    .query_by_label(&t!("admin.kick_title", name = "Ann"))
                    .is_some()
            );
            harness.state_mut().as_mut().unwrap().servers[0]
                .dir
                .members
                .edit()
                .reverse();
            harness.run_steps(4);
            harness.get_by_label(t!("common.cancel")).click();
            harness.run_steps(4);
            assert!(drain_kicks(&mut sent).is_empty());

            choose_kick(&mut harness, "Bob");
            harness.get_by_label(t!("admin.kick")).click();
            harness.run_steps(4);
            assert_eq!(drain_kicks(&mut sent), vec![BOB]);
            assert!(harness.state().as_ref().unwrap().kick.is_none());
            harness.run_steps(4);
            assert!(drain_kicks(&mut sent).is_empty());
            assert!(drain_kicks(&mut other).is_empty());
        }
    }

    #[test]
    fn kick_confirmation_rechecks_live_authority_and_connection() {
        for change in 0..7 {
            let (mut harness, mut sent, mut other) = kick_harness(false);
            choose_kick(&mut harness, "Ann");
            let server = &mut harness.state_mut().as_mut().unwrap().servers[0];
            match change {
                0 => server.dir.perms.kick = false,
                1 => server.dir.members.edit().retain(|m| m.person != ANN),
                2 => server
                    .dir
                    .members
                    .edit()
                    .iter_mut()
                    .find(|m| m.person == ANN)
                    .unwrap()
                    .roles
                    .push("owner".into()),
                3 => server.status = Status::Offline,
                4 => server.known.server_id = [99; 32],
                5 => {
                    let (commands, _) = tokio::sync::mpsc::unbounded_channel();
                    server.commands = commands;
                }
                _ => server.dir.members.edit().retain(|m| m.person != ME),
            }
            harness.run_steps(4);
            assert!(harness.state().as_ref().unwrap().kick.is_none());
            assert!(drain_kicks(&mut sent).is_empty());
            assert!(drain_kicks(&mut other).is_empty());
        }
    }

    #[test]
    fn kick_confirmation_expires_on_reconnect_between_frames() {
        let (mut harness, mut sent, mut other) = kick_harness(false);
        choose_kick(&mut harness, "Ann");
        let ctx = harness.ctx.clone();
        let home = harness.state_mut().as_mut().unwrap();
        for index in [1, 0] {
            let address = home.servers[index].known.address.clone();
            for status in [Status::Offline, Status::Online] {
                home.apply(
                    &ctx,
                    crate::net::Update::Status(address.clone(), status),
                    0.0,
                );
            }
            assert_eq!(home.kick.is_some(), index == 1);
        }
        harness.run_steps(4);
        assert!(harness.query_by_label(t!("admin.kick")).is_none());
        assert!(drain_kicks(&mut sent).is_empty());
        assert!(drain_kicks(&mut other).is_empty());
    }

    #[test]
    fn open_member_menu_cannot_follow_a_reused_row() {
        let (mut harness, mut sent, _) = kick_harness(false);
        harness
            .get_by_role_and_label(eframe::egui::accesskit::Role::Button, "Ann")
            .click_secondary();
        harness.run_steps(4);
        harness.state_mut().as_mut().unwrap().servers[0]
            .dir
            .members
            .edit()
            .reverse();
        harness.run_steps(4);
        // A positional widget can disappear when reordered; it may close, never retarget.
        if let Some(kick) = harness.query_by_label(t!("admin.kick")) {
            kick.click();
            harness.run_steps(4);
            assert!(
                harness
                    .query_by_label(&t!("admin.kick_title", name = "Ann"))
                    .is_some()
            );
        }
        assert!(drain_kicks(&mut sent).is_empty());
    }

    #[test]
    fn member_menu_reopening_selects_only_the_new_person() {
        let (mut harness, mut sent, _) = kick_harness(false);
        harness
            .get_by_role_and_label(eframe::egui::accesskit::Role::Button, "Ann")
            .click_secondary();
        harness.run_steps(4);
        harness
            .get_by_role_and_label(eframe::egui::accesskit::Role::Button, "Bob")
            .click_secondary();
        harness.run_steps(4);
        // Outside right-click closes the old menu; reopen if necessary.
        if harness.query_by_label(t!("admin.kick")).is_none() {
            harness
                .get_by_role_and_label(eframe::egui::accesskit::Role::Button, "Bob")
                .click_secondary();
            harness.run_steps(4);
        }
        harness.get_by_label(t!("admin.kick")).click();
        harness.run_steps(4);
        assert!(
            harness
                .query_by_label(&t!("admin.kick_title", name = "Bob"))
                .is_some()
        );
        harness.get_by_label(t!("common.cancel")).click();
        harness.run_steps(4);
        assert!(drain_kicks(&mut sent).is_empty());
    }

    #[test]
    fn kick_confirmation_keeps_origin_after_server_switch_and_repeat_clicks() {
        let (mut harness, mut sent, mut other) = kick_harness(false);
        choose_kick(&mut harness, "Ann");
        let home = harness.state_mut().as_mut().unwrap();
        // A second action cannot replace a question already on screen.
        admin::moderate(home, 0, BOB, Moderate::Kick);
        home.servers.swap(0, 1);
        harness.run_steps(4);
        harness.get_by_label(t!("admin.kick")).click();
        harness.get_by_label(t!("admin.kick")).click();
        harness.run_steps(4);
        assert_eq!(drain_kicks(&mut sent), vec![ANN]);
        assert!(drain_kicks(&mut other).is_empty());
    }

    #[test]
    fn member_settings_actions_also_open_the_dedicated_confirmation() {
        let (mut harness, mut sent, _) = kick_harness(true);
        harness
            .get_by_label(&t!("admin.member_actions", name = "Ann"))
            .click();
        harness.run_steps(4);
        harness.get_by_label(t!("admin.kick")).click();
        harness.run_steps(4);
        assert!(
            harness
                .query_by_label(&t!("admin.kick_title", name = "Ann"))
                .is_some()
        );
        assert!(drain_kicks(&mut sent).is_empty());
        harness.get_by_label(t!("admin.kick")).click();
        harness.run_steps(4);
        assert_eq!(drain_kicks(&mut sent), vec![ANN]);
        assert!(harness.state().as_ref().unwrap().server_settings.is_some());
    }

    #[test]
    fn kick_confirmation_dark_and_light() {
        for dark in [true, false] {
            let (mut harness, mut sent, _) = kick_harness(true);
            harness.ctx.set_theme(if dark {
                eframe::egui::Theme::Dark
            } else {
                eframe::egui::Theme::Light
            });
            harness.run_steps(4);
            choose_kick(&mut harness, "Ann");
            assert!(
                harness
                    .query_by_label(&t!("admin.kick_title", name = "Ann"))
                    .is_some()
            );
            if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
                std::fs::create_dir_all(&dir).unwrap();
                let theme = if dark { "dark" } else { "light" };
                harness
                    .render()
                    .unwrap()
                    .save(format!("{dir}/kick-{theme}.png"))
                    .unwrap();
            }
            assert!(drain_kicks(&mut sent).is_empty());
        }
    }

    fn shown(harness: &Harness<'static, Option<Home>>, label: &str) -> bool {
        harness.query_by_label(label).is_some()
    }

    fn moderator() -> Perms {
        Perms {
            kick: true,
            ban: true,
            manage_roles: true,
            mute_members: true,
            move_members: true,
            ..Perms::default()
        }
    }

    /// Mute for everyone, Remove from channel, Change roles, Kick, Ban.
    fn moderation_shown(harness: &Harness<'static, Option<Home>>) -> Vec<bool> {
        [
            t!("call.mute_everyone"),
            t!("call.remove_from_channel"),
            t!("admin.change_roles"),
            t!("admin.kick"),
            t!("admin.ban"),
        ]
        .iter()
        .map(|label| shown(harness, label))
        .collect()
    }

    #[test]
    fn your_own_menu_has_details_and_edit_profile_only() {
        let harness = menu(Case {
            perms: moderator(),
            person: ME,
            in_voice: false,
            in_call: false,
            ann_roles: &[],
        });
        assert!(shown(&harness, t!("common.details")));
        assert!(shown(&harness, t!("settings.edit_profile")));
        assert!(!shown(&harness, t!("home.message")));
        assert!(!shown(&harness, t!("pokes.poke")));
        assert!(!shown(&harness, t!("friends.add")));
        assert_eq!(moderation_shown(&harness), [false; 5]);
    }

    #[test]
    fn a_member_sees_message_poke_and_friends_but_no_moderation() {
        let harness = menu(Case {
            perms: Perms::default(),
            person: ANN,
            in_voice: true,
            in_call: false,
            ann_roles: &[],
        });
        for label in [
            t!("home.message"),
            t!("pokes.poke"),
            t!("common.details"),
            t!("friends.add"),
            t!("friends.block"),
        ] {
            assert!(shown(&harness, label), "{label}");
        }
        assert!(!shown(&harness, t!("settings.edit_profile")));
        assert!(!shown(&harness, t!("call.mute")));
        assert_eq!(moderation_shown(&harness), [false; 5]);
    }

    #[test]
    fn a_moderator_sees_voice_moderation_only_while_they_are_in_voice() {
        let in_voice = menu(Case {
            perms: moderator(),
            person: ANN,
            in_voice: true,
            in_call: false,
            ann_roles: &[],
        });
        assert_eq!(moderation_shown(&in_voice), [true; 5]);
        assert!(shown(&in_voice, t!("home.message")));
        // Not in your call: no volume.
        assert!(!shown(&in_voice, t!("call.mute")));

        let elsewhere = menu(Case {
            perms: moderator(),
            person: ANN,
            in_voice: false,
            in_call: false,
            ann_roles: &[],
        });
        assert_eq!(
            moderation_shown(&elsewhere),
            [false, false, true, true, true]
        );
    }

    #[test]
    fn in_a_call_together_the_menu_has_their_volume() {
        let harness = menu(Case {
            perms: Perms::default(),
            person: ANN,
            in_voice: true,
            in_call: true,
            ann_roles: &[],
        });
        assert!(shown(&harness, t!("call.mute")));
        assert!(shown(&harness, t!("home.message")));
        assert_eq!(moderation_shown(&harness), [false; 5]);
    }

    #[test]
    fn the_mixer_has_a_row_per_app_and_links_them() {
        let mut harness = menu(Case {
            perms: Perms::default(),
            person: ANN,
            in_voice: true,
            in_call: true,
            ann_roles: &[],
        });
        let home = harness.state_mut().as_mut().unwrap();
        let peer = &mut home.servers[0].voice.get_mut(&LOUNGE).unwrap()[0];
        let sid = |s: &str| s.to_owned().try_into().unwrap();
        peer.apps = vec![
            (sid("TR_a"), "Spotify".into()),
            (sid("TR_b"), "Game".into()),
        ];
        harness.run_steps(4);
        // Each row's name is both its label and its slider's.
        let any = |harness: &Harness<'static, Option<Home>>, label| {
            harness.query_all_by_label(label).next().is_some()
        };
        for label in [t!("call.voice"), "Spotify", "Game"] {
            assert!(any(&harness, label), "{label}");
        }
        assert!(!any(&harness, t!("call.all_apps")));
        let switch = eframe::egui::accesskit::Role::Switch;
        harness
            .get_by_role_and_label(switch, t!("call.link"))
            .click();
        harness.run_steps(4);
        // The menu stays open for the mixer.
        assert!(any(&harness, t!("call.all_apps")));
        let audio = &harness.state().as_ref().unwrap().voice.audio;
        assert_eq!(audio.mix().person(&ANN).link, Some(1.0));
        // Its items still close it.
        harness.get_by_label(t!("pokes.poke")).click();
        harness.run_steps(4);
        assert!(!any(&harness, t!("call.all_apps")));
    }

    /// Moderation shown against Ann with `roles`, by someone of `rank`.
    fn moderation_against(rank: i64, roles: &'static [&'static str]) -> Vec<bool> {
        let harness = menu(Case {
            perms: Perms {
                rank,
                ..moderator()
            },
            person: ANN,
            in_voice: true,
            in_call: false,
            ann_roles: roles,
        });
        moderation_shown(&harness)
    }

    #[test]
    fn moderation_needs_a_strictly_higher_rank_and_never_reaches_the_owner() {
        // Lower: an admin over a moderator, the owner over an admin.
        assert_eq!(moderation_against(900, &["mod"]), [true; 5]);
        assert_eq!(moderation_against(1000, &["admin", "mod"]), [true; 5]);
        // Equal and higher.
        assert_eq!(moderation_against(500, &["mod"]), [false; 5]);
        assert_eq!(moderation_against(500, &["admin"]), [false; 5]);
        // The owner, even for another owner rank.
        assert_eq!(moderation_against(1000, &["owner"]), [false; 5]);
        // Message and Poke stay for everyone.
        let harness = menu(Case {
            perms: Perms {
                rank: 500,
                ..moderator()
            },
            person: ANN,
            in_voice: true,
            in_call: false,
            ann_roles: &["owner"],
        });
        assert!(harness.query_by_label(t!("home.message")).is_some());
    }
}
