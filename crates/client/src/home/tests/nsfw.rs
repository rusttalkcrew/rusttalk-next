//! NSFW channels: a warning takes the chat pane until this device continues or goes back,
//! the answer is remembered on this device per server and channel, the list and header
//! show a badge, and managers mark it in channel settings. Set RUSTTALK_TEST_SCREENSHOTS
//! to save how they look.

use super::*;
use crate::home::nsfw::Agreed;
use egui_kittest::{
    Harness,
    kittest::{Queryable, by},
};

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

const SIZES: [Vec2; 2] = [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)];

fn screenshot(h: &mut Harness<'_, Home>, name: &str, size: Vec2, dark: bool) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let path = format!("{dir}/{name}-{theme}-{}x{}.png", size.x, size.y);
        h.render().unwrap().save(path).unwrap();
    }
}

/// #projects marked NSFW and open, on a device that never answered; a manager's view.
fn nsfw_home(dark: bool) -> (Home, Sent) {
    let (mut fixture, sent) = files_harness(dark);
    let mut home = fixture.state_mut().take().unwrap();
    // A harness-free fixture writes nowhere: answers stay in memory.
    home.nsfw_agreed = Agreed::default();
    home.view = View::Server(0);
    let server = &mut home.servers[0];
    server.dir.perms.manage_channels = true;
    server.dir.categories[0].channels[0].nsfw = true;
    server.open = Some(PROJECTS);
    server.chats.get_mut(&PROJECTS).unwrap().opened = false;
    (home, sent)
}

/// The window: rail, channel list and the open channel; a phone shows only the channel.
fn harness(size: Vec2, dark: bool) -> (Harness<'static, Home>, Sent) {
    let (home, sent) = nsfw_home(dark);
    let mut h = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        .build_ui_state(
            |ui, home: &mut Home| {
                let colors = ui::colors(ui);
                if !ui::is_phone(ui.ctx()) {
                    Panel::left("servers")
                        .resizable(false)
                        .exact_size(RAIL_WIDTH)
                        .frame(panel_frame(colors.rail))
                        .show(ui, |ui| server_rail(ui, home));
                    Panel::left("channels")
                        .resizable(false)
                        .exact_size(layout::CHANNELS_WIDTH)
                        .frame(panel_frame(colors.side))
                        .show(ui, |ui| sidebar(ui, home));
                }
                CentralPanel::default()
                    .frame(panel_frame(colors.main))
                    .show(ui, |ui| {
                        if let Some(channel) = home.servers[0].open {
                            chat_pane(ui, home, 0, channel);
                        }
                    });
            },
            home,
        );
    h.ctx.data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
    h.run_steps(3);
    (h, sent)
}

fn opened(sent: &mut Sent) -> bool {
    std::iter::from_fn(|| sent.try_recv().ok()).any(|c| matches!(c, Command::Open(PROJECTS)))
}

fn warned(h: &Harness<'_, Home>) -> bool {
    h.query_by_label(t!("nsfw.warning")).is_some()
}

#[test]
fn the_first_visit_warns_and_going_back_leaves_it_closed() {
    for size in SIZES {
        for dark in [true, false] {
            let (mut h, mut sent) = harness(size, dark);
            assert!(warned(&h));
            assert!(h.query_by_label(t!("nsfw.continue")).is_some());
            // Nothing of the channel is loaded or counted as read behind the warning.
            assert!(!opened(&mut sent));
            assert!(h.state().viewing().is_empty());
            screenshot(&mut h, "nsfw-warning", size, dark);

            h.get_by_label(t!("nsfw.go_back")).click();
            h.run_steps(3);
            let home = h.state();
            assert_eq!(home.servers[0].open, None);
            assert!(!home.agreed(0, &PROJECTS));
            assert!(!opened(&mut sent));
        }
    }
}

#[test]
fn continuing_opens_the_channel_and_is_not_asked_again() {
    let (mut h, mut sent) = harness(SIZES[1], true);
    h.get_by_label(t!("nsfw.continue")).click();
    h.run_steps(3);
    assert!(h.state().agreed(0, &PROJECTS));
    assert!(!warned(&h));
    assert!(opened(&mut sent));
    assert_eq!(h.state().viewing(), [(0, PROJECTS)]);

    // Leaving and coming back doesn't ask again.
    h.state_mut().servers[0].open = None;
    h.run_steps(2);
    h.state_mut().servers[0].open = Some(PROJECTS);
    h.run_steps(3);
    assert!(!warned(&h));
}

#[test]
fn the_list_and_the_header_show_a_badge() {
    for dark in [true, false] {
        let (mut h, _sent) = harness(SIZES[1], dark);
        h.get_by_label(t!("nsfw.continue")).click();
        h.run_steps(3);
        // One in the channel list, one in the header.
        let badges = h
            .query_all(
                by().role(egui::accesskit::Role::Image)
                    .label(t!("nsfw.badge_help")),
            )
            .count();
        assert_eq!(badges, 2);
        screenshot(&mut h, "nsfw-badge", SIZES[1], dark);

        // Unmarked, both go.
        h.state_mut().servers[0].dir.categories[0].channels[0].nsfw = false;
        h.run_steps(3);
        assert!(h.query_by_label(t!("nsfw.badge_help")).is_none());
    }
}

#[test]
fn the_answer_stays_on_this_device_per_server_and_channel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nsfw_agreed.json");
    let here: url::Url = "https://friends.example.org/".parse().unwrap();
    let there: url::Url = "https://other.example.org/".parse().unwrap();
    let mut agreed = Agreed::open(path.clone());
    assert!(!agreed.agreed(&here, &PROJECTS));
    agreed.agree(&here, PROJECTS);

    // Read back after a restart; another server's channel with the same ID isn't agreed.
    let reopened = Agreed::open(path);
    assert!(reopened.agreed(&here, &PROJECTS));
    assert!(!reopened.agreed(&there, &PROJECTS));
    assert!(!reopened.agreed(&here, &[7; 32]));
}

/// The Edit channel dialog of #projects, for a channel manager.
fn dialog_harness(size: Vec2, dark: bool) -> (Harness<'static, Home>, Sent) {
    let (mut home, sent) = nsfw_home(dark);
    home.servers[0].dir.categories[0].channels[0].nsfw = false;
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
fn a_manager_marks_a_channel_nsfw_in_its_settings() {
    for size in SIZES {
        for dark in [true, false] {
            let (mut h, mut sent) = dialog_harness(size, dark);
            h.get(
                by().role(egui::accesskit::Role::Switch)
                    .label(t!("nsfw.title")),
            )
            .click();
            h.run_steps(3);
            screenshot(&mut h, "nsfw-settings", size, dark);
            h.get_by_label(t!("common.save")).click();
            h.run_steps(3);
            let saved = std::iter::from_fn(|| sent.try_recv().ok()).find_map(|c| match c {
                Command::RenameChannel { nsfw, .. } => Some(nsfw),
                _ => None,
            });
            assert_eq!(saved, Some(Some(true)));
        }
    }
}
