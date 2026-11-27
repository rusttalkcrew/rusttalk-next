//! New member rules: Server settings > New members saves a preset or a custom length, and a
//! new member sees a note over the message box and can't attach files.

use super::*;
use crate::net::{Admin, AdminUpdate};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};
use rusttalk_core::auth::NewMemberRules;

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

/// The chat, or with `rules` Server settings > New members showing them as saved.
fn screen(
    size: Vec2,
    dark: bool,
    rules: Option<NewMemberRules>,
    new_member_until: Option<i64>,
) -> (Harness<'static, Option<Screen>>, Sent) {
    let (mut files, sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.find.opened = false;
    home.servers[0].dir.new_member_until = new_member_until;
    if let Some(rules) = rules {
        home.servers[0].dir.perms.manage_server = true;
        crate::admin::open(&mut home, 0, crate::admin::Tab::NewMembers);
        let settings = home.server_settings.as_mut().unwrap();
        settings.apply(AdminUpdate::NewMembers(Some(rules)));
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
    let off = by()
        .role(Role::RadioButton)
        .label(t!("admin.new_members_off"));
    if rules.is_some() && harness.query(off).is_none() {
        // Phones may start on the page list.
        harness.get_by_label(t!("admin.new_members")).click();
        harness.run_steps(4);
    }
    (harness, sent)
}

fn click(h: &mut Harness<'_, Option<Screen>>, role: Role, label: &str) {
    h.get(by().role(role).label(label)).click();
    h.run_steps(4);
}

fn sent_saves(sent: &mut Sent) -> Vec<i64> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Admin(Admin::SaveNewMembers(secs)) => Some(secs),
            _ => None,
        })
        .collect()
}

fn save_enabled(h: &Harness<'_, Option<Screen>>) -> bool {
    !h.get(by().role(Role::Button).label(t!("common.save")))
        .accesskit_node()
        .is_disabled()
}

#[test]
fn owners_pick_a_length_and_new_members_see_what_waits() {
    let screenshots = std::env::var("RUSTTALK_TEST_SCREENSHOTS").ok();
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let theme = if dark { "dark" } else { "light" };
            let shot = |h: &mut Harness<'static, Option<Screen>>, name: &str| {
                if let Some(dir) = &screenshots {
                    h.render()
                        .unwrap()
                        .save(format!("{dir}/{name}-{}x{}-{theme}.png", size.x, size.y))
                        .unwrap();
                }
            };
            let (mut h, mut sent) = screen(size, dark, Some(NewMemberRules { secs: 0 }), None);
            assert!(!save_enabled(&h));
            click(&mut h, Role::RadioButton, t!("admin.new_members_1_h"));
            assert!(save_enabled(&h));
            shot(&mut h, "new-members");
            click(&mut h, Role::Button, t!("common.save"));
            assert_eq!(sent_saves(&mut sent), [3600]);

            // A custom length is in minutes.
            click(&mut h, Role::RadioButton, t!("admin.new_members_custom"));
            let minutes = by()
                .role(Role::SpinButton)
                .label(t!("admin.new_members_minutes"));
            assert!(h.query(minutes).is_some());
            shot(&mut h, "new-members-custom");

            // A new member: a note says when things unlock, and there is nothing to attach.
            let until = crate::notify::unix_now() + 3600;
            let (mut h, _) = screen(size, dark, None, Some(until));
            let note = t!("new_members.note", time = super::super::time(until));
            if h.query_by_label_contains(&note).is_none() {
                // Phones start on the channel list.
                h.get_by_label_contains("projects").click();
                h.run_steps(6);
            }
            assert!(h.query_by_label_contains(&note).is_some());
            click(&mut h, Role::Button, t!("home.attach_files"));
            let upload = h.get_by_label_contains(t!("home.upload_file"));
            assert!(upload.accesskit_node().is_disabled());
            h.key_press(egui::Key::Escape);
            h.run_steps(4);
            shot(&mut h, "new-members-note");

            // Once the rules end, the note goes.
            let (h, _) = screen(size, dark, None, Some(crate::notify::unix_now() - 1));
            assert!(h.query_by_label_contains(&note).is_none());
        }
    }
}
