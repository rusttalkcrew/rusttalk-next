//! Asking someone to join your voice channel (#294): sending, receiving and the pop-up.

use std::time::Duration;

use eframe::egui::{self, Align, Label, Layout, Ui};
use rusttalk_core::messages::{VoiceAnswer, VoiceRequest};

use super::{Home, View};
use crate::{
    call,
    friends::Relation,
    net::{Command, Draft},
    notify::{self, Arrival, Mention},
    server::{CallState, Id, Msg, Server, Status},
    ui,
    voice_requests::{Incoming, JoinCheck, State},
};

/// How long a pop-up that just appeared ignores clicks, in seconds.
const ARM: f64 = 0.5;

/// What was clicked on a request.
enum Choice {
    Answer(VoiceAnswer),
    Mute,
}

/// Whether this device is in a call on `server`, so it can ask people there over.
pub fn calling(voice: &call::Voice, server: &Server) -> bool {
    (voice.call.as_ref())
        .is_some_and(|call| call.server == server.address && call.state == CallState::Connected)
}

impl Home {
    /// Ask `person` into the voice channel this device is in on server `index`, and
    /// ring the call.
    pub(super) fn ask_to_join(&mut self, index: usize, person: Id, now: f64) {
        // The menu can stay open while the call ends or the person is asked.
        if !calling(&self.voice, &self.servers[index]) || self.requests.waiting(&person) {
            return;
        }
        let Some(channel) = self.voice.call.as_ref().map(|call| call.channel) else {
            return;
        };
        let online = self.servers[index].status == Status::Online;
        let (true, Ok(id)) = (online, rusttalk_core::keys::random_key()) else {
            self.toast = Some((t!("home.waiting_try_again").into(), now + 6.0));
            return;
        };
        let sent = notify::unix_now();
        self.send_request(index, person, *id, VoiceRequest::Ask(channel, sent));
        let name = self.servers[index].member_name(&person);
        self.requests.asked(*id, person, name, sent);
        self.servers[index].send(Command::Ring);
        self.voice.audio.ring();
    }

    /// Only on the channel's own server, which the other person needs anyway.
    fn send_request(&self, index: usize, person: Id, id: Id, request: VoiceRequest) {
        self.servers[index].send(Command::Send(Draft {
            forum_post: false,
            id,
            channel: [0; 32],
            peer: Some(person),
            text: String::new(),
            reply_to: None,
            edit_of: None,
            revision: 0,
            files: Vec::new(),
            keep: Vec::new(),
            sticker: None,
            friend: None,
            poke: false,
            poll: None,
            voice_request: Some(request),
            preview: None,
            inline: Vec::new(),
            queued: Default::default(),
        }));
    }

    /// A live request or answer in the DM with `peer` on server `index`. Like pokes,
    /// requests follow blocks, "friends only" and that DM's notification settings.
    pub(super) fn request_arrived(
        &mut self,
        ctx: &egui::Context,
        (index, dm): (usize, Id),
        peer: Id,
        msg: &Msg,
    ) {
        let crate::server::Body::VoiceRequest(request) = msg.body else {
            return;
        };
        let now = notify::unix_now();
        if msg.person != peer {
            // Answered on another of this person's devices.
            if msg.person == *self.identity.certificate.person_id()
                && let VoiceRequest::Answer(id, _) = request
            {
                self.requests.answered_elsewhere(peer, id, msg.at, now);
            }
            return;
        }
        if self.friends.relation(&peer) == Relation::Blocked {
            return;
        }
        let (channel, sent) = match request {
            VoiceRequest::Ask(channel, sent) => (channel, sent),
            VoiceRequest::Answer(id, answer) => {
                self.requests.answered(&peer, &id, answer, now);
                ctx.request_repaint();
                return;
            }
        };
        if self.friends.is_request(&peer, self.friends_only()) {
            return;
        }
        let server = &self.servers[index];
        let prefs = self.notify.prefs();
        let rule = prefs.effective(&server.address, &dm, true, now);
        if rule.muted || rule.level == notify::Level::Nothing {
            return;
        }
        // Only channels this person can open are in their list.
        let Some(found) = server.channel(&channel).filter(|found| found.voice) else {
            return;
        };
        if self.voice.in_call(server, &channel) {
            return;
        }
        let incoming = Incoming {
            id: msg.id,
            person: peer,
            name: server.member_name(&peer),
            server: server.address.clone(),
            channel,
            channel_name: found.name.clone(),
            until: 0,
            check: JoinCheck::Idle,
        };
        let title = t!("voice_request.asks_you", name = incoming.name);
        let body = format!("🔊 {}", incoming.channel_name);
        let target = (server.address.clone(), channel);
        if !self.requests.take(incoming, sent, now) {
            return;
        }
        // A request calls on this person, like a mention.
        let arrival = Arrival {
            dm: true,
            mention: Mention::Me,
        };
        if notify::should_notify(prefs, rule, arrival, self.focused, false) {
            self.notify.show_silent(ctx, &title, &body, target);
        }
        self.voice.audio.ring();
        ctx.request_repaint();
    }

    /// `person` rang the call this device is in.
    pub(super) fn call_rang(&self, server: &url::Url, channel: Id, person: Id) {
        let here = (self.voice.call.as_ref())
            .is_some_and(|call| call.server == *server && call.channel == channel);
        if here
            && self.friends.relation(&person) != Relation::Blocked
            && !self.requests.muted(&person, notify::unix_now())
        {
            self.voice.audio.ring();
        }
    }

    fn answer(&mut self, request: &Incoming, answer: VoiceAnswer, now: f64) {
        let index = (self.index(&request.server))
            .filter(|index| self.servers[*index].status == Status::Online);
        let (Some(index), Ok(id)) = (index, rusttalk_core::keys::random_key()) else {
            self.toast = Some((t!("home.waiting_try_again").into(), now + 6.0));
            return;
        };
        if answer == VoiceAnswer::Joining {
            if request.check != JoinCheck::Idle {
                return;
            }
            if !self.servers[index].is_voice(&request.channel)
                || request.until <= notify::unix_now()
            {
                self.requests.close(&request.person, &request.id);
                return;
            }
            #[cfg(any(target_os = "android", target_os = "ios"))]
            if !self.voice.in_call(&self.servers[index], &request.channel) {
                call::join(self, index, request.channel);
                self.voice.pending_invitation = Some(request.id);
                return;
            }
            self.check_voice_destination(index, request.id);
            return;
        }
        self.requests.close(&request.person, &request.id);
        let reply = VoiceRequest::Answer(request.id, answer);
        self.send_request(index, request.person, *id, reply);
    }

    pub(crate) fn check_voice_destination(&mut self, index: usize, id: Id) {
        if let Some(request) = self.requests.incoming.iter_mut().find(|r| {
            r.id == id && r.server == self.servers[index].address && r.check == JoinCheck::Idle
        }) {
            request.check = if self.voice.in_call(&self.servers[index], &request.channel) {
                JoinCheck::AlreadyThere
            } else {
                JoinCheck::Join
            };
            self.servers[index].send(Command::CheckVoiceDestination {
                request: id,
                channel: request.channel,
            });
        }
    }

    pub(super) fn voice_destination(
        &mut self,
        address: &url::Url,
        id: Id,
        allowed: bool,
        now: f64,
    ) {
        let Some(index) = self.index(address) else {
            return;
        };
        let Some(request) = self
            .requests
            .incoming
            .iter()
            .find(|r| r.id == id && r.server == *address && r.check != JoinCheck::Idle)
            .cloned()
        else {
            return;
        };
        self.requests.close(&request.person, &id);
        if !allowed
            || request.until <= notify::unix_now()
            || self.servers[index].status != Status::Online
            || !self.servers[index].is_voice(&request.channel)
        {
            self.toast = Some((t!("home.waiting_try_again").into(), now + 6.0));
            return;
        }
        let Ok(reply) = rusttalk_core::keys::random_key() else {
            return;
        };
        if !self.voice.in_call(&self.servers[index], &request.channel)
            && (request.check == JoinCheck::AlreadyThere
                || !call::join_ready(self, index, request.channel, now))
        {
            return;
        }
        self.send_request(
            index,
            request.person,
            *reply,
            VoiceRequest::Answer(id, VoiceAnswer::Joining),
        );
        self.view = View::Server(index);
        self.layout.list = false;
    }
}

/// The pop-up at the top right: the request that rings, and how this person's own went.
/// Returns whether it shows, as it takes the poke pop-up's place.
pub fn popup(ctx: &egui::Context, home: &mut Home) -> bool {
    let (now, time) = (notify::unix_now(), ctx.input(|input| input.time));
    home.requests.expire(now);
    let request = home.requests.incoming.first().cloned();
    if let Some(request) = &request
        && let Some(index) = home.index(&request.server)
        && (home.voice).in_call(&home.servers[index], &request.channel)
    {
        // They got there by themselves, which answers it as well.
        home.answer(request, VoiceAnswer::Joining, time);
        return true;
    }
    if request.is_none() && home.requests.outgoing.is_empty() {
        return false;
    }
    // Both run out by the clock.
    ctx.request_repaint_after(Duration::from_millis(500));
    let armed = (request.as_ref())
        .is_some_and(|request| time - home.requests.on_top_since(request.id, time) >= ARM);

    let (mut choice, mut dismissed) = (None, None);
    ui::toast(ctx, "voice-request", |ui| {
        if let Some(request) = &request {
            choice = card(ui, request, armed);
        }
        for (at, sent) in home.requests.outgoing.iter().enumerate() {
            if at > 0 || request.is_some() {
                ui.separator();
            }
            let name = &sent.name;
            let text = match sent.state {
                State::Waiting => t!("voice_request.asking", name = name),
                State::Answered(VoiceAnswer::Joining) => t!("voice_request.joining", name = name),
                State::Answered(VoiceAnswer::Declined) => t!("voice_request.declined", name = name),
                State::Answered(VoiceAnswer::Later) => {
                    t!("voice_request.will_come_later", name = name)
                }
                State::NoAnswer => t!("voice_request.no_answer", name = name),
            };
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                // A request can't be taken back, so only its outcome closes.
                if sent.state != State::Waiting
                    && ui::small_close(ui, t!("common.dismiss")).clicked()
                {
                    dismissed = Some(sent.id);
                }
                ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
                    ui.add(Label::new(ui::bold(text)).wrap());
                });
            });
        }
    });
    if let Some(id) = dismissed {
        home.requests.outgoing.retain(|sent| sent.id != id);
    }
    if let (Some(request), Some(choice)) = (&request, choice) {
        match choice {
            Choice::Answer(answer) => home.answer(request, answer, time),
            Choice::Mute => home.requests.mute(request.person, now),
        }
    }
    true
}

/// Who asks and where, the three answers and, below them, muting.
fn card(ui: &mut Ui, request: &Incoming, armed: bool) -> Option<Choice> {
    let title = t!("voice_request.asks_you", name = request.name);
    ui.add(Label::new(ui::bold(title)).wrap());
    let channel = format!("🔊 {}", request.channel_name);
    ui.add(Label::new(crate::emoji::text(&channel)).truncate());
    ui.add_space(ui::SPACE_S);
    let mut choice = None;
    ui.add_enabled_ui(armed, |ui| {
        // Three in a row don't fit the card in every language.
        ui.horizontal(|ui| {
            if ui::primary(ui, t!("voice_request.join")).clicked() {
                choice = Some(Choice::Answer(VoiceAnswer::Joining));
            }
            if ui::secondary(ui, t!("voice_request.decline")).clicked() {
                choice = Some(Choice::Answer(VoiceAnswer::Declined));
            }
        });
        if ui::secondary(ui, t!("voice_request.later")).clicked() {
            choice = Some(Choice::Answer(VoiceAnswer::Later));
        }
        ui.separator();
        if ui::secondary(ui, t!("voice_request.mute")).clicked() {
            choice = Some(Choice::Mute);
        }
    });
    choice
}

#[cfg(test)]
mod tests {
    use eframe::egui::Vec2;
    use egui_kittest::{Harness, kittest::Queryable};
    use rusttalk_core::presence::Presence;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::server::{Call, Category, Channel, Member, Peer};

    const ME: Id = [1; 32];
    const LOUNGE: Id = [2; 32];
    const ANN: Id = [7; 32];
    const DM: Id = [9; 32];

    pub(super) type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);

    /// Me in the Lounge on an online server, Ann outside it, and what the window sent.
    pub(super) fn window(
        size: Vec2,
        dark: bool,
    ) -> (Harness<'static, Option<Screen>>, UnboundedReceiver<Command>) {
        let mut harness = Harness::builder()
            .with_size(size)
            // Short frames, so the pop-up stays unarmed for a few.
            .with_step_dt(0.05)
            .build_ui_state(
                |ui, screen: &mut Option<Screen>| {
                    if let Some((home, updates, keystore)) = screen {
                        super::super::show(ui, home, updates, keystore);
                    }
                },
                None,
            );
        ui::install(&harness.ctx);
        let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Desktop").unwrap();
        let data = tempfile::tempdir().unwrap().keep();
        let mut home = Home::new(identity, &harness.ctx, data.clone()).unwrap();
        let mut settings = home.voice.audio.settings();
        settings.theme_mode = if dark { "dark" } else { "light" }.into();
        // Quiet: a test may ring.
        settings.call_sounds.volume = 0.01;
        home.voice.audio.set_settings(settings);
        // Never a real desktop notification from a test.
        let mut prefs = home.notify.prefs().clone();
        prefs.desktop = false;
        home.notify.set_prefs(prefs);
        let (commands, sent) = tokio::sync::mpsc::unbounded_channel();
        let known = rusttalk_core::auth::KnownServer {
            address: "https://friends.example.org/".parse().unwrap(),
            server_id: [2; 32],
            tls_fingerprint: None,
        };
        let mut server = super::super::new_server(known, "Friends", commands);
        server.status = Status::Online;
        server.dir.me = ME;
        let member = |person, name: &str| Member {
            person,
            name: Some(name.into()),
            devices: Vec::new(),
            roles: Vec::new(),
            avatar: None,
            presence: Presence::Online,
            hide_offline: false,
            bio: None,
            voice_muted: false,
        };
        server.dir.members = vec![member(ME, "Alex"), member(ANN, "Ann")].into();
        server.dir.dms = vec![crate::server::Dm {
            channel: DM,
            peer: ANN,
        }];
        let channel = |id: u8, name: &str, voice| Channel {
            id: [id; 32],
            name: name.into(),
            topic: String::new(),
            voice,
            page: false,
            forum: false,
            audio_kbps: 64,
            member_limit: 0,
            focus_until: None,
            flags: Vec::new(),
        };
        server.dir.categories = vec![Category {
            id: None,
            name: String::new(),
            channels: vec![channel(1, "general", false), channel(2, "Lounge", true)],
        }];
        let me = Peer {
            person: ME,
            connecting: false,
            unknown: false,
            status: Default::default(),
            apps: Vec::new(),
            screens: Vec::new(),
            camera: None,
            terminal: None,
        };
        server.voice.insert(LOUNGE, vec![me]);
        server.open = Some([1; 32]);
        home.voice.call = Some(Call {
            server: server.address.clone(),
            channel: LOUNGE,
            state: CallState::Connected,
            stats: Default::default(),
            can_talk: true,
            can_share_video: true,
        });
        home.servers.push(server);
        home.view = View::Server(0);
        *harness.state_mut() = Some((
            home,
            crate::update::Updater::new(&data),
            crate::keystore::without_keychain(&data),
        ));
        harness.run_steps(4);
        (harness, sent)
    }

    pub(super) fn home<'a>(harness: &'a mut Harness<'static, Option<Screen>>) -> &'a mut Home {
        &mut harness.state_mut().as_mut().unwrap().0
    }

    /// Ann asks this person into the Lounge.
    pub(super) fn ann_asks(harness: &mut Harness<'static, Option<Screen>>, id: u8) {
        let ctx = harness.ctx.clone();
        let msg = Msg {
            backfilled: false,
            id: [id; 32],
            person: ANN,
            at: notify::unix_now(),
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            body: crate::server::Body::VoiceRequest(VoiceRequest::Ask(LOUNGE, notify::unix_now())),
            files: Vec::new(),
            live: None,
            sticker: None,
            replicas: Vec::new(),
            copy_files: Vec::new(),
        };
        let home = home(harness);
        // Ann asks from her own call, so this person is elsewhere.
        home.voice.call = None;
        home.arrived(&ctx, 0, DM, msg);
        harness.run_steps(2);
    }

    fn sent_request(sent: &mut UnboundedReceiver<Command>) -> Option<VoiceRequest> {
        std::iter::from_fn(|| sent.try_recv().ok()).find_map(|command| match command {
            Command::Send(draft) => draft.voice_request,
            _ => None,
        })
    }

    #[test]
    fn asking_sends_the_request_and_rings_the_call() {
        let (mut harness, mut sent) = window(Vec2::new(900.0, 600.0), true);
        home(&mut harness).ask_to_join(0, ANN, 0.0);
        harness.run_steps(2);
        let commands: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
        assert!(commands.iter().any(|command| matches!(
            command,
            Command::Send(Draft {
                peer: Some(ANN),
                voice_request: Some(VoiceRequest::Ask(LOUNGE, _)),
                ..
            })
        )));
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, Command::Ring))
        );
        assert!(harness.query_by_label_contains("Asking Ann").is_some());
        // Once is enough until she answers.
        home(&mut harness).ask_to_join(0, ANN, 0.0);
        assert!(sent.try_recv().is_err());
    }

    #[test]
    fn join_now_answers_and_joins_but_not_right_away() {
        let (mut harness, mut sent) = window(Vec2::new(900.0, 600.0), true);
        ann_asks(&mut harness, 3);
        assert!(
            harness
                .query_by_label_contains("Ann asks you to join")
                .is_some()
        );
        // A click as the pop-up appears does nothing.
        harness.get_by_label("Join now").click();
        harness.run_steps(1);
        assert!(sent_request(&mut sent).is_none());
        harness.run_steps(12);
        harness.get_by_label("Join now").click();
        harness.run_steps(2);
        let commands: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
        assert!(commands.iter().any(|c| matches!(
            c,
            Command::CheckVoiceDestination {
                channel: LOUNGE,
                ..
            }
        )));
        assert!(
            !commands
                .iter()
                .any(|c| matches!(c, Command::JoinVoice(_) | Command::Send(_)))
        );
        let address = home(&mut harness).servers[0].address.clone();
        home(&mut harness).voice_destination(&address, [3; 32], true, 1.0);
        let commands: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
        assert!(commands.iter().any(|command| matches!(command,
            Command::Send(Draft { voice_request: Some(VoiceRequest::Answer(id, VoiceAnswer::Joining)), .. }) if *id == [3; 32])));
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, Command::JoinVoice(LOUNGE)))
        );
        assert!(home(&mut harness).requests.incoming.is_empty());
    }

    #[test]
    fn newer_call_decision_wins_over_delayed_join_approval() {
        for switch in [false, true] {
            let (mut harness, mut sent) = window(Vec2::new(420.0, 720.0), false);
            let mut current = home(&mut harness).voice.call.take().unwrap();
            current.channel = [5; 32];
            ann_asks(&mut harness, 3);
            let home = home(&mut harness);
            home.voice.call = Some(current);
            let address = home.servers[0].address.clone();
            home.check_voice_destination(0, [3; 32]);
            if switch {
                call::join(home, 0, [6; 32]);
            } else {
                call::leave(home);
            }
            while sent.try_recv().is_ok() {}
            home.voice_destination(&address, [3; 32], true, 1.0);
            assert_eq!(
                home.voice.call.as_ref().map(|call| call.channel),
                switch.then_some([6; 32])
            );
            assert!(sent.try_recv().is_err());
        }
    }

    #[test]
    fn failed_destination_check_keeps_the_current_call() {
        let (mut harness, mut sent) = window(Vec2::new(420.0, 720.0), false);
        let mut current = home(&mut harness).voice.call.take().unwrap();
        current.channel = [5; 32];
        ann_asks(&mut harness, 3);
        let home = home(&mut harness);
        home.voice.call = Some(current);
        let address = home.servers[0].address.clone();
        home.check_voice_destination(0, [3; 32]);
        home.voice_destination(&address, [3; 32], false, 1.0);
        assert_eq!(home.voice.call.as_ref().unwrap().channel, [5; 32]);
        assert!(home.requests.incoming.is_empty());
        assert!(
            !std::iter::from_fn(|| sent.try_recv().ok()).any(|c| matches!(
                c,
                Command::JoinVoice(_) | Command::LeaveVoice | Command::Send(_)
            ))
        );
    }

    #[test]
    fn verified_pages_reconcile_answers_and_cached_asks_stay_silent() {
        let (mut harness, _) = window(Vec2::new(900.0, 600.0), true);
        let ctx = harness.ctx.clone();
        ann_asks(&mut harness, 3);
        let home = home(&mut harness);
        let address = home.servers[0].address.clone();
        let now = notify::unix_now();
        let message = |person, request| Msg {
            backfilled: false,
            id: [4; 32],
            person,
            at: now,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            body: crate::server::Body::VoiceRequest(request),
            files: vec![],
            live: None,
            sticker: None,
            replicas: vec![],
            copy_files: vec![],
        };
        let other_ask = message(ANN, VoiceRequest::Ask(LOUNGE, now));
        let answer = VoiceRequest::Answer([3; 32], VoiceAnswer::Later);
        let mine = *home.identity.certificate.person_id();
        home.requests.asked([3; 32], ANN, String::new(), now);
        home.apply(
            &ctx,
            crate::net::Update::Cached(
                address.clone(),
                Box::new(home.servers[0].dir.clone()),
                vec![(DM, vec![other_ask.clone(), message(ANN, answer)])],
            ),
            0.0,
        );
        assert!(home.requests.waiting(&ANN));
        assert_eq!(home.requests.incoming[0].id, [3; 32]);
        home.apply(
            &ctx,
            crate::net::Update::Messages {
                server: address,
                channel: DM,
                items: vec![message(mine, answer), other_ask, message(ANN, answer)],
                older: false,
                page: Some((true, Some([4; 32]))),
            },
            0.0,
        );
        assert!(home.requests.incoming.is_empty());
        assert_eq!(
            home.requests.outgoing[0].state,
            State::Answered(VoiceAnswer::Later)
        );
    }

    #[test]
    fn muting_closes_the_request_sends_nothing_and_keeps_her_quiet() {
        let (mut harness, mut sent) = window(Vec2::new(900.0, 600.0), true);
        ann_asks(&mut harness, 3);
        harness.run_steps(12);
        harness.get_by_label("Mute for 15 min").click();
        harness.run_steps(2);
        assert!(sent_request(&mut sent).is_none());
        ann_asks(&mut harness, 4);
        assert!(home(&mut harness).requests.incoming.is_empty());
    }
}
