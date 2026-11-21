//! Voice channel rows in the sidebar: a click opens the chat, the Join button or a
//! double-click joins. Driven off-screen with egui_kittest.

use eframe::egui::Vec2;
use egui_kittest::{Harness, kittest::Queryable};
use tokio::sync::mpsc::UnboundedReceiver;

use super::{Home, View, channel_list, new_server};
use crate::{
    net::Command,
    server::{Category, Channel, Id, Status},
    ui,
};

const LOUNGE: Id = [2; 32];

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
    let mut server = new_server(
        "https://friends.example.org/".parse().unwrap(),
        "Friends",
        commands,
    );
    server.status = Status::Online;
    let channel = |id: u8, name: &str, voice| Channel {
        id: [id; 32],
        name: name.into(),
        topic: String::new(),
        voice,
        audio_kbps: 64,
        flags: Vec::new(),
    };
    server.dir.categories = vec![Category {
        id: None,
        name: String::new(),
        channels: vec![channel(1, "general", false), channel(2, "Lounge", true)],
    }];
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
