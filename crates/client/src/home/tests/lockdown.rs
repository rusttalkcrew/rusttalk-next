//! Raid tools (#665): Server settings > Lockdown locks and lifts in one click, the banner
//! tells the people who may lift it, and a ban can remove recent messages.

use super::*;
use crate::{net::Admin, server::Lockdown};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

fn lockdown(locked: bool, automatic: bool) -> Lockdown {
    Lockdown {
        locked,
        automatic,
        message: Some("Back in an hour, sorry!".into()),
        auto_joins: 10,
        auto_secs: 120,
    }
}

/// The whole screen as a moderator who may lock the server down, optionally with server
/// settings open on `tab`.
fn screen(
    size: Vec2,
    dark: bool,
    saved: Lockdown,
    tab: Option<crate::admin::Tab>,
) -> (Harness<'static, Option<Screen>>, Sent) {
    let (mut files, sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.find.opened = false;
    let perms = &mut home.servers[0].dir.perms;
    (perms.lock_down, perms.ban, perms.remove_recent_messages) = (true, true, true);
    perms.rank = 50;
    home.servers[0].dir.lockdown = Some(saved);
    if let Some(tab) = tab {
        crate::admin::open(&mut home, 0, tab);
    }
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

fn sent_admin(sent: &mut Sent) -> Vec<Admin> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            // Opening settings loads what the page shows.
            Command::Admin(Admin::Load { .. }) => None,
            Command::Admin(admin) => Some(admin),
            _ => None,
        })
        .collect()
}

fn shot(h: &mut Harness<'static, Option<Screen>>, name: &str, size: Vec2, dark: bool) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        let theme = if dark { "dark" } else { "light" };
        h.render()
            .unwrap()
            .save(format!("{dir}/{name}-{}x{}-{theme}.png", size.x, size.y))
            .unwrap();
    }
}

#[test]
fn lockdown_locks_and_lifts_in_one_click() {
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let (mut h, mut sent) = screen(
                size,
                dark,
                lockdown(false, false),
                Some(crate::admin::Tab::Lockdown),
            );
            if h.query_by_label(t!("lockdown.lock")).is_none() {
                // Phones may start on the page list.
                h.get_by_label(t!("lockdown.title")).click();
                h.run_steps(4);
            }
            shot(&mut h, "lockdown-settings", size, dark);
            let save = by().role(Role::Button).label(t!("common.save"));
            assert!(h.get(save).accesskit_node().is_disabled());
            h.get_by_label(t!("lockdown.lock")).click();
            h.run_steps(4);
            let sent = sent_admin(&mut sent);
            let [Admin::SaveLockdown(locked)] = sent.as_slice() else {
                panic!("lock sends one save");
            };
            assert_eq!(
                *locked,
                Lockdown {
                    automatic: false,
                    ..lockdown(true, false)
                }
            );
        }
    }
}

#[test]
fn an_automatic_lockdown_shows_a_banner_with_lift() {
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let (mut h, mut sent) = screen(size, dark, lockdown(true, true), None);
            if h.query_by_label(t!("lockdown.banner_automatic")).is_none() {
                // Phones start on the channel list.
                h.state_mut().as_mut().unwrap().0.layout.list = false;
                h.run_steps(6);
            }
            assert!(h.query_by_label(t!("lockdown.banner_automatic")).is_some());
            shot(&mut h, "lockdown-banner", size, dark);
            h.get(by().role(Role::Button).label(t!("lockdown.lift")))
                .click();
            h.run_steps(4);
            let sent = sent_admin(&mut sent);
            let [Admin::SaveLockdown(lifted)] = sent.as_slice() else {
                panic!("lift sends one save");
            };
            assert!(!lifted.locked);
            assert_eq!(lifted.message, lockdown(true, true).message);
        }
    }
    // People who can't lift it see nothing.
    let (mut h, _) = screen(Vec2::new(1100.0, 700.0), true, lockdown(true, true), None);
    h.state_mut().as_mut().unwrap().0.servers[0]
        .dir
        .perms
        .lock_down = false;
    h.run_steps(4);
    assert!(h.query_by_label(t!("lockdown.banner_automatic")).is_none());
}

#[test]
fn a_ban_offers_to_remove_recent_messages() {
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let (mut h, mut sent) = screen(size, dark, lockdown(false, false), None);
            let home = &mut h.state_mut().as_mut().unwrap().0;
            crate::admin::moderate(home, 0, ANN, crate::admin::Moderate::Ban);
            h.run_steps(6);
            let remove = by().role(Role::ComboBox).label(t!("admin.ban_remove"));
            h.get(remove).click();
            h.run_steps(4);
            h.get_by_label(t!("admin.ban_remove_day")).click();
            h.run_steps(4);
            shot(&mut h, "ban-remove-recent", size, dark);
            h.get(by().role(Role::Button).label(t!("admin.ban")))
                .click();
            h.run_steps(4);
            let sent = sent_admin(&mut sent);
            assert!(
                matches!(sent.as_slice(), [Admin::Ban(person, false, Some(86_400))] if *person == ANN),
                "ban asks to remove a day"
            );
        }
    }
}
