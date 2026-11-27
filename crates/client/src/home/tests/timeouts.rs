//! Timeouts (#663): a moderator picks a length and reason, and the member sees a banner
//! in place of the message box until it ends. Set RUSTTALK_TEST_SCREENSHOTS to save how
//! they look.

use super::*;
use crate::net::Admin;
use egui_kittest::{Harness, kittest::Queryable};

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

/// The whole app on the Friends server's #projects, as `change` leaves it.
fn screen(
    size: Vec2,
    dark: bool,
    change: impl FnOnce(&mut Home),
) -> (Harness<'static, Option<Screen>>, Sent) {
    let (mut files, sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.find.opened = false;
    // Phones show the chat, not the channel list.
    home.layout.list = false;
    change(&mut home);
    let data = tempfile::tempdir().unwrap();
    let mut harness = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        .build_ui_state(
            |ui, screen: &mut Option<Screen>| {
                if let Some((home, updates, keystore)) = screen {
                    show(ui, home, updates, keystore);
                }
            },
            None,
        );
    *harness.state_mut() = Some((
        home,
        crate::update::Updater::new(data.path()),
        crate::keystore::without_keychain(data.path()),
    ));
    harness.run_steps(6);
    (harness, sent)
}

fn screenshot(h: &mut Harness<'_, Option<Screen>>, name: &str, size: Vec2, dark: bool) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let path = format!("{dir}/{name}-{theme}-{}x{}.png", size.x, size.y);
        h.render().unwrap().save(path).unwrap();
    }
}

const SIZES: [Vec2; 2] = [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)];

#[test]
fn a_timed_out_member_sees_until_when_instead_of_the_message_box() {
    let until = crate::notify::unix_now() + 600;
    let banner = t!("timeout.banner", time = crate::admin::timeout::clock(until));
    for size in SIZES {
        for dark in [true, false] {
            let (mut h, _) = screen(size, dark, |home| {
                let server = &mut home.servers[0];
                server.dir.members.edit()[0].timeout_until = Some(until);
                server.dir.timeout_reason = Some("Please take a short break".into());
            });
            assert!(h.query_by_label(&banner).is_some(), "banner at {size:?}");
            assert!(h.query_by_label(t!("timeout.banner_help")).is_some());
            screenshot(&mut h, "timeout-banner", size, dark);
        }
    }
    // Once it has ended the message box is back.
    let (h, _) = screen(SIZES[0], true, |home| {
        home.servers[0].dir.members.edit()[0].timeout_until = Some(until - 1200);
    });
    assert!(h.query_by_label(&banner).is_none());
}

#[test]
fn a_moderator_picks_a_length_and_reason() {
    let title = t!("timeout.title", name = "Ann");
    for size in SIZES {
        for dark in [true, false] {
            let (mut h, mut sent) = screen(size, dark, |home| {
                let perms = &mut home.servers[0].dir.perms;
                perms.timeout_members = true;
                perms.rank = 50;
                crate::admin::timeout::open(home, 0, ANN);
            });
            assert!(h.query_by_label(&title).is_some(), "dialog at {size:?}");
            h.get_by_label(t!("timeout.length_1h")).click();
            h.run_steps(3);
            screenshot(&mut h, "timeout-dialog", size, dark);
            h.get_by_label(t!("timeout.confirm")).click();
            h.run_steps(3);
            let picked: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok())
                .filter_map(|command| match command {
                    Command::Admin(Admin::Timeout {
                        person,
                        seconds,
                        reason,
                    }) => Some((person, seconds, reason)),
                    _ => None,
                })
                .collect();
            assert_eq!(picked, [(ANN, 3600, None)]);
            assert!(h.query_by_label(&title).is_none());
        }
    }
}
