//! Server settings > Joining: Save only with a change, and fresh values on a revisit.

use super::*;
use crate::net::{Admin, AdminUpdate};
use egui::accesskit::Toggled;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

/// The whole screen at `size`, on Server settings > Joining as an admin, with the
/// server's joining settings loaded. Also returns the commands the view sends.
fn screen(size: Vec2, dark: bool) -> (Harness<'static, Option<Screen>>, Sent) {
    let (mut files, sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    // Never a search on the real directory from a test.
    home.find.opened = false;
    home.servers[0].dir.perms.admin = true;
    crate::admin::open(&mut home, 0, crate::admin::Tab::Joining);
    let settings = home.server_settings.as_mut().unwrap();
    settings.apply(AdminUpdate::Joining(false, 250));
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
    if size.x < ui::PHONE_WIDTH {
        // Phones start on the page list when Joining is the first page.
        harness.get_by_label(t!("admin.joining")).click();
        harness.run_steps(4);
    }
    (harness, sent)
}

fn save_enabled(h: &Harness<'_, Option<Screen>>) -> bool {
    !h.get(by().role(Role::Button).label(t!("common.save")))
        .accesskit_node()
        .is_disabled()
}

/// Open joining and the member limit, as the page shows them.
fn shown(h: &Harness<'_, Option<Screen>>) -> (bool, f64) {
    let open = h
        .get(by().role(Role::Switch).label(t!("admin.open_joining")))
        .accesskit_node()
        .toggled()
        == Some(Toggled::True);
    let cap = h
        .get(by().role(Role::SpinButton).label(t!("admin.member_limit")))
        .accesskit_node()
        .numeric_value()
        .unwrap();
    (open, cap)
}

fn apply(h: &mut Harness<'_, Option<Screen>>, update: AdminUpdate) {
    let home = &mut h.state_mut().as_mut().unwrap().0;
    home.server_settings.as_mut().unwrap().apply(update);
    h.run_steps(3);
}

fn toggle_open(h: &mut Harness<'_, Option<Screen>>) {
    h.get(by().role(Role::Switch).label(t!("admin.open_joining")))
        .click();
    h.run_steps(3);
}

fn sent_admin(sent: &mut Sent) -> Vec<Admin> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Admin(admin) => Some(admin),
            _ => None,
        })
        .collect()
}

#[test]
fn joining_save_waits_for_a_change() {
    for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
        for dark in [true, false] {
            let (mut h, mut sent) = screen(size, dark);
            assert!(!save_enabled(&h), "Save is on with nothing changed");
            if let Some(dir) = std::env::var_os("RUSTTALK_JOINING_SCREENSHOTS") {
                let theme = if dark { "dark" } else { "light" };
                let name = format!("joining-{theme}-{}x{}.png", size.x, size.y);
                h.render()
                    .unwrap()
                    .save(std::path::PathBuf::from(dir).join(name))
                    .unwrap();
            }

            // A change turns Save on; switching it back turns it off again.
            toggle_open(&mut h);
            assert!(save_enabled(&h));
            toggle_open(&mut h);
            assert!(!save_enabled(&h));

            toggle_open(&mut h);
            sent_admin(&mut sent);
            h.get(by().role(Role::Button).label(t!("common.save")))
                .click();
            h.run_steps(3);
            assert!(
                (sent_admin(&mut sent).iter())
                    .any(|admin| matches!(admin, Admin::Joining(true, 250)))
            );
            // Saved: the server's answer turns Save off until the next change.
            apply(&mut h, AdminUpdate::Joining(true, 250));
            assert!(!save_enabled(&h));
            assert_eq!(shown(&h), (true, 250.0));
        }
    }
}

#[test]
fn joining_reloads_on_a_revisit_but_keeps_unsaved_edits() {
    let (mut h, mut sent) = screen(Vec2::new(1100.0, 700.0), true);
    // Back on Joining from another page: it asks the server again.
    h.get_by_label(t!("admin.files")).click();
    h.run_steps(3);
    sent_admin(&mut sent);
    h.get_by_label(t!("admin.joining")).click();
    h.run_steps(3);
    assert!(
        (sent_admin(&mut sent).iter())
            .any(|admin| matches!(admin, Admin::Load { joining: true, .. }))
    );

    // Someone else changed it: the page shows the server's new values.
    apply(&mut h, AdminUpdate::Joining(true, 500));
    assert_eq!(shown(&h), (true, 500.0));
    assert!(!save_enabled(&h));

    // An edit waiting for Save stays when the page loads again.
    toggle_open(&mut h);
    apply(&mut h, AdminUpdate::Joining(true, 900));
    assert_eq!(shown(&h), (false, 500.0));
    assert!(save_enabled(&h));
}
