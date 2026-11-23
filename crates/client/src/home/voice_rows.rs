//! Voice channel rows in the sidebar: a click opens the chat, the Join button or a
//! double-click joins, and people under it show their card on a click and their menu
//! on a right click. Driven off-screen with egui_kittest.

use eframe::egui::{Vec2, accesskit::Role};
use egui_kittest::{Harness, kittest::Queryable};
use rusttalk_core::presence::Presence;
use tokio::sync::mpsc::UnboundedReceiver;

use super::{Home, View, channel_list, new_server};
use crate::{
    net::Command,
    server::{Category, Channel, Id, Member, Peer, Status},
    ui,
};

const LOUNGE: Id = [2; 32];
const ANN: Id = [7; 32];

/// An online server with a text channel and a voice channel, shown in the sidebar.
/// Also returns what the window sent to the server.
fn window() -> (Harness<'static, Option<Home>>, UnboundedReceiver<Command>) {
    let mut harness = Harness::builder()
        .with_size(Vec2::new(260.0, 300.0))
        // Two clicks this close count as a double-click.
        .with_step_dt(0.05)
        .build_ui_state(
            |ui, home: &mut Option<Home>| {
                if let Some(home) = home {
                    channel_list(ui, home, 0);
                }
            },
            None,
        );
    ui::install(&harness.ctx);
    let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Desktop").unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut home = Home::new(identity, &harness.ctx, data.path().to_owned()).unwrap();
    let (commands, sent) = tokio::sync::mpsc::unbounded_channel();
    let known = rusttalk_core::auth::KnownServer {
        address: "https://friends.example.org/".parse().unwrap(),
        server_id: [2; 32],
        tls_fingerprint: None,
    };
    let mut server = new_server(known, "Friends", commands);
    server.status = Status::Online;
    let channel = |id: u8, name: &str, voice| Channel {
        id: [id; 32],
        name: name.into(),
        topic: String::new(),
        voice,
        page: false,
        audio_kbps: 64,
        flags: Vec::new(),
    };
    server.dir.categories = vec![Category {
        id: None,
        name: String::new(),
        channels: vec![channel(1, "general", false), channel(2, "Lounge", true)],
    }];
    server.dir.members = vec![Member {
        person: ANN,
        name: Some("Ann".into()),
        devices: Vec::new(),
        roles: Vec::new(),
        avatar: None,
        presence: Presence::Online,
        hide_offline: false,
        bio: None,
        voice_muted: false,
    }];
    let ann = Peer {
        person: ANN,
        connecting: false,
        unknown: false,
        status: Default::default(),
        apps: Vec::new(),
        screens: Vec::new(),
        camera: None,
        terminal: None,
    };
    server.voice.insert(LOUNGE, vec![ann]);
    home.servers.push(server);
    home.view = View::Server(0);
    *harness.state_mut() = Some(home);
    harness.run_steps(10);
    (harness, sent)
}

fn home<'a>(harness: &'a Harness<'static, Option<Home>>) -> &'a Home {
    harness.state().as_ref().unwrap()
}

#[test]
fn channel_hover_cursor_is_consistent_for_members_and_managers() {
    let (mut harness, _sent) = window();
    for manage in [false, true] {
        let server = &mut harness.state_mut().as_mut().unwrap().servers[0];
        server.dir.perms.manage_channels = manage;
        server.dir.categories[0].id = Some([3; 32]);
        harness.run_steps(3);
        for label in ["#  general", "🔊  Lounge"] {
            harness.get_by_label(label).hover();
            harness.run_steps(3);
            assert_eq!(
                harness.output().platform_output.cursor_icon,
                eframe::egui::CursorIcon::PointingHand
            );
        }
    }
}

#[test]
fn clicking_a_voice_channel_opens_its_chat_without_joining() {
    let (mut harness, mut sent) = window();
    harness.get_by_label("🔊  Lounge").click();
    harness.run_steps(10);
    assert_eq!(home(&harness).servers[0].open, Some(LOUNGE));
    assert!(home(&harness).voice.call.is_none());
    assert!(sent.try_recv().is_err());
}

#[test]
fn join_button_shows_on_hover_and_on_the_open_row_and_joins() {
    let (mut harness, mut sent) = window();
    assert!(harness.query_by_label("Join voice").is_none());
    harness.get_by_label("#  general").hover();
    harness.run_steps(10);
    assert!(harness.query_by_label("Join voice").is_none());
    harness.get_by_label("🔊  Lounge").hover();
    harness.run_steps(10);
    assert!(harness.query_by_label("Join voice").is_some());
    // Open and with the pointer elsewhere, the button stays.
    harness.get_by_label("🔊  Lounge").click();
    harness.get_by_label("#  general").hover();
    harness.run_steps(10);
    harness.get_by_label("Join voice").click();
    harness.run_steps(10);
    assert!(home(&harness).voice.call.is_some());
    assert!(matches!(sent.try_recv(), Ok(Command::JoinVoice(id)) if id == LOUNGE));
    // Already in this call: nothing left to join.
    assert!(harness.query_by_label("Join voice").is_none());
}

#[test]
fn double_clicking_a_voice_channel_joins() {
    let (mut harness, mut sent) = window();
    harness.get_by_label("🔊  Lounge").click();
    harness.step();
    harness.get_by_label("🔊  Lounge").click();
    harness.run_steps(10);
    assert!(home(&harness).voice.call.is_some());
    assert!(matches!(sent.try_recv(), Ok(Command::JoinVoice(id)) if id == LOUNGE));
}

#[test]
fn clicking_someone_in_voice_shows_their_card_and_right_click_their_menu() {
    let (mut harness, _sent) = window();
    harness.get_by_role_and_label(Role::Button, "Ann").click();
    harness.run_steps(4);
    assert!(harness.query_by_label("Message").is_some());
    assert!(harness.query_by_label("Details").is_none());

    let (mut harness, _sent) = window();
    harness
        .get_by_role_and_label(Role::Button, "Ann")
        .click_secondary();
    harness.run_steps(4);
    assert!(harness.query_by_label("Details").is_some());
}
