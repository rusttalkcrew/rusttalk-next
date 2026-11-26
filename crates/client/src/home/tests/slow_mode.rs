//! Slow mode: after a message, the message box counts down and Send waits; people who
//! may bypass it never wait; managers pick it in channel settings. Set
//! RUSTTALK_TEST_SCREENSHOTS to save how they look.

use super::*;
use crate::net::Update;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

const SIZES: [Vec2; 2] = [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)];

fn screenshot<T>(h: &mut Harness<'_, T>, name: &str, size: Vec2, dark: bool) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let path = format!("{dir}/{name}-{theme}-{}x{}.png", size.x, size.y);
        h.render().unwrap().save(path).unwrap();
    }
}

/// The #projects chat with 30 s slow mode, and a draft ready to send.
fn slow_chat(dark: bool, size: Vec2, bypass: bool) -> (Harness<'static, Option<Home>>, Sent) {
    let (mut h, sent) = files_harness(dark);
    h.ctx.data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
    h.set_size(size);
    let server = &mut h.state_mut().as_mut().unwrap().servers[0];
    server.dir.categories[0].channels[0].slow_mode_secs = 30;
    server.dir.perms.bypass_slow_mode = bypass;
    let chat = server.chats.get_mut(&PROJECTS).unwrap();
    chat.at_start = true;
    chat.draft = "Count me in".into();
    h.run_steps(3);
    (h, sent)
}

fn sent_texts(sent: &mut Sent) -> Vec<String> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Send(draft) => Some(draft.text),
            _ => None,
        })
        .collect()
}

/// The countdown line as it should read now, and whether it shows.
fn countdown_shown(h: &Harness<'static, Option<Home>>) -> bool {
    let chat = &h.state().as_ref().unwrap().servers[0].chats[&PROJECTS];
    let now = h.ctx.input(|input| input.time);
    crate::home::slow_mode::left(chat, now).is_some_and(|seconds| {
        h.query_by_label(&t!("slow_mode.wait", seconds = seconds))
            .is_some()
    })
}

fn send_disabled(h: &Harness<'static, Option<Home>>) -> bool {
    h.get_by_label(t!("home.send"))
        .accesskit_node()
        .is_disabled()
}

#[test]
fn after_a_message_the_box_counts_down_and_send_waits() {
    let tip = t!(
        "slow_mode.header_tip",
        time = t!("slow_mode.seconds", seconds = 30)
    );
    for size in SIZES {
        for dark in [true, false] {
            let (mut h, mut sent) = slow_chat(dark, size, false);
            // The header says slow mode is on.
            assert!(h.query_by_label(&tip).is_some());
            assert!(!countdown_shown(&h));
            h.get_by_label(t!("home.send")).click_accesskit();
            h.run_steps(3);
            assert_eq!(sent_texts(&mut sent), ["Count me in"]);

            // The next one waits, Send is off and Enter doesn't send it.
            let home = h.state_mut().as_mut().unwrap();
            home.servers[0].chats.get_mut(&PROJECTS).unwrap().draft = "And Ann too".into();
            h.run_steps(3);
            assert!(countdown_shown(&h));
            assert!(send_disabled(&h));
            h.get(by().role(egui::accesskit::Role::MultilineTextInput))
                .focus();
            h.run_steps(1);
            h.key_press(egui::Key::Enter);
            h.run_steps(3);
            assert!(sent_texts(&mut sent).is_empty());
            screenshot(&mut h, "slow-mode-countdown", size, dark);

            // Once the wait is over it can go.
            let home = h.state_mut().as_mut().unwrap();
            home.servers[0].chats.get_mut(&PROJECTS).unwrap().slow_until = Some(0.0);
            h.run_steps(3);
            assert!(!countdown_shown(&h));
            assert!(!send_disabled(&h));
        }
    }
}

#[test]
fn people_who_may_bypass_it_never_wait() {
    let (mut h, mut sent) = slow_chat(true, SIZES[1], true);
    h.get_by_label(t!("home.send")).click_accesskit();
    h.run_steps(3);
    assert_eq!(sent_texts(&mut sent), ["Count me in"]);
    let home = h.state_mut().as_mut().unwrap();
    home.servers[0].chats.get_mut(&PROJECTS).unwrap().draft = "And Ann too".into();
    h.run_steps(3);
    let chat = &h.state().as_ref().unwrap().servers[0].chats[&PROJECTS];
    assert!(chat.slow_until.is_none());
    assert!(!send_disabled(&h));
}

#[test]
fn a_refusal_from_the_server_shows_the_time_left() {
    let (mut h, _sent) = slow_chat(true, SIZES[1], false);
    let ctx = h.ctx.clone();
    let home = h.state_mut().as_mut().unwrap();
    let address = home.servers[0].address.clone();
    let now = ctx.input(|input| input.time);
    home.apply(&ctx, Update::SlowMode(address, PROJECTS, 12), now);
    h.run_steps(1);
    assert!(countdown_shown(&h));
    let chat = &h.state().as_ref().unwrap().servers[0].chats[&PROJECTS];
    assert_eq!(chat.slow_until, Some(now + 12.0));
    assert!(send_disabled(&h));
}

/// The Edit channel dialog of #projects, for a channel manager.
fn dialog_harness(size: Vec2, dark: bool) -> (Harness<'static, Home>, Sent) {
    let (mut fixture, sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].dir.perms.manage_channels = true;
    home.edit_channel(0, PROJECTS, ChannelEdit::Rename);
    let mut h = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        // The channel name field has no accessible name yet (not this test's concern).
        .with_accessibility_check(false)
        .build_ui_state(|ui, home| dialogs(ui.ctx(), home), home);
    h.run_steps(3);
    (h, sent)
}

#[test]
fn a_manager_picks_slow_mode_in_channel_settings() {
    let five = t!("slow_mode.minutes", minutes = 5);
    for size in SIZES {
        for dark in [true, false] {
            let (mut h, mut sent) = dialog_harness(size, dark);
            let combo = by()
                .role(egui::accesskit::Role::ComboBox)
                .label(t!("slow_mode.title"));
            h.get(combo).click();
            h.run_steps(3);
            h.get_by_label(&five).click();
            h.run_steps(3);
            screenshot(&mut h, "slow-mode-settings", size, dark);
            h.get_by_label(t!("common.save")).click();
            h.run_steps(3);
            let saved = std::iter::from_fn(|| sent.try_recv().ok()).find_map(|c| match c {
                Command::RenameChannel { slow_mode_secs, .. } => slow_mode_secs,
                _ => None,
            });
            assert_eq!(saved, Some(300));
        }
    }
}

#[test]
fn choices_read_short() {
    let labels: Vec<_> = crate::home::slow_mode::CHOICES
        .into_iter()
        .map(crate::home::slow_mode::label)
        .collect();
    assert_eq!(
        labels,
        [
            "Off", "5 s", "10 s", "30 s", "1 min", "5 min", "15 min", "1 h", "6 h"
        ]
    );
}
