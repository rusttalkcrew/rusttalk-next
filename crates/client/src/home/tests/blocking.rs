//! Blocking someone (#667): their messages in a shared channel fold into a
//! "Blocked message" line until shown, never mark or notify, and Settings → Privacy
//! lists them with Unblock.

use super::*;
use crate::friends::Act;
use egui_kittest::{Harness, kittest::Queryable};

type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);

const SECRET: &str = "Want to see something?";
const MINE: &str = "Hello everyone";

fn text(id: u8, person: Id, at: i64, text: &str) -> Msg {
    Msg {
        backfilled: false,
        id: [id; 32],
        person,
        at,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        body: Body::Text(text.into()),
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: Vec::new(),
        copy_files: Vec::new(),
    }
}

/// The whole screen at `size` in the projects channel, with Ann blocked and one
/// message from each of them.
fn screen(size: Vec2, dark: bool, set: impl FnOnce(&mut Home)) -> Harness<'static, Option<Screen>> {
    let (mut files, _sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.layout.list = false;
    home.find.opened = false;
    let mut prefs = home.notify.prefs().clone();
    prefs.desktop = false;
    home.notify.set_prefs(prefs);
    let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
    chat.at_start = true;
    chat.merge(
        vec![
            text(1, ME, 1_700_000_000, MINE),
            text(2, ANN, 1_700_000_100, SECRET),
        ],
        false,
    );
    home.friends.act(ANN, Act::Block, "Ann", 1);
    set(&mut home);
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
    harness
}

fn home<'h>(h: &'h Harness<'_, Option<Screen>>) -> &'h Home {
    &h.state().as_ref().unwrap().0
}

fn shows(h: &Harness<'_, Option<Screen>>, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}

fn screenshot(h: &mut Harness<'_, Option<Screen>>, name: &str, size: Vec2, dark: bool) {
    if let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let path = format!("{dir}/{name}-{theme}-{}x{}.png", size.x, size.y);
        h.render().unwrap().save(path).unwrap();
    }
}

#[test]
fn a_blocked_persons_message_stays_folded_until_shown() {
    let size = Vec2::new(1100.0, 700.0);
    let mut h = screen(size, true, |_| {});
    assert!(shows(&h, MINE));
    assert!(!shows(&h, SECRET));
    assert!(shows(&h, t!("home.blocked_message")));
    h.get_by_label(t!("home.show_blocked_message")).click();
    h.run_steps(6);
    assert!(shows(&h, SECRET));
    assert!(!shows(&h, t!("home.blocked_message")));
}

#[test]
fn a_blocked_persons_channel_message_does_not_mark_the_channel() {
    let mut h = screen(Vec2::new(1100.0, 700.0), true, |home| {
        home.view = View::Direct(None);
    });
    let ctx = h.ctx.clone();
    let address = home(&h).servers[0].address.clone();
    let home = &mut h.state_mut().as_mut().unwrap().0;
    let now = crate::notify::unix_now();
    home.arrived(&ctx, 0, PROJECTS, text(3, ANN, now + 60, "Hey"));
    assert!(!home.notify.unread(&address, &PROJECTS));
    home.friends.act(ANN, Act::Unblock, "Ann", 2);
    home.arrived(&ctx, 0, PROJECTS, text(4, ANN, now + 120, "Hey"));
    assert!(home.notify.unread(&address, &PROJECTS));
}

#[test]
fn privacy_settings_list_blocked_people_with_unblock() {
    let size = Vec2::new(1100.0, 700.0);
    let mut h = screen(size, true, |home| home.settings = Some(Tab::Privacy));
    assert!(shows(&h, t!("friends.blocked_section")));
    assert!(shows(&h, "Ann"));
    h.get_by_label(t!("friends.unblock")).click();
    h.run_steps(6);
    assert!(!home(&h).is_blocked(&ANN));
    assert!(shows(&h, t!("friends.empty_blocked")));
}

#[test]
fn blocking_screenshots() {
    for size in [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)] {
        for dark in [true, false] {
            let mut h = screen(size, dark, |_| {});
            screenshot(&mut h, "blocked-message", size, dark);
            drop(h);
            let mut h = screen(size, dark, |home| home.settings = Some(Tab::Privacy));
            screenshot(&mut h, "privacy-blocked", size, dark);
            drop(h);
            // Ann's card, unblocked, with its Block button.
            let mut h = screen(size, dark, |home| {
                home.friends.act(ANN, Act::Unblock, "Ann", 2);
            });
            h.query_all_by_label("Ann").next().unwrap().click();
            h.run_steps(6);
            assert!(shows(&h, t!("friends.block")));
            screenshot(&mut h, "card-block", size, dark);
            drop(h);
            // The ban question with "Also block them for me".
            let mut h = screen(size, dark, |home| {
                home.servers[0].dir.perms.ban = true;
                home.servers[0].dir.perms.rank = 500;
                crate::admin::moderate(home, 0, ANN, crate::admin::Moderate::Ban);
            });
            assert!(shows(&h, t!("admin.ban_also_block")));
            screenshot(&mut h, "ban-also-block", size, dark);
        }
    }
}
