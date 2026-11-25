//! Server settings > Message history: keeping everything saves at once, a lower number asks
//! first, and a host cap hides "Keep all messages".

use super::*;
use crate::net::{Admin, AdminUpdate};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};
use rusttalk_core::auth::MessageHistory;

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

fn screen(
    size: Vec2,
    dark: bool,
    saved: MessageHistory,
) -> (Harness<'static, Option<Screen>>, Sent) {
    let (mut files, sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.find.opened = false;
    home.servers[0].dir.perms.manage_server = true;
    crate::admin::open(&mut home, 0, crate::admin::Tab::History);
    let settings = home.server_settings.as_mut().unwrap();
    settings.apply(AdminUpdate::History(Some(saved)));
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
    let count = by().role(Role::SpinButton).label(t!("admin.history_count"));
    if harness.query(count).is_none() && harness.query(keep_all()).is_none() {
        // Phones may start on the page list.
        harness.get_by_label(t!("admin.history")).click();
        harness.run_steps(4);
    }
    (harness, sent)
}

fn keep_all<'a>() -> egui_kittest::kittest::By<'a> {
    by().role(Role::Switch).label(t!("admin.history_keep_all"))
}

fn click(h: &mut Harness<'_, Option<Screen>>, role: Role, label: &str) {
    h.get(by().role(role).label(label)).click();
    h.run_steps(4);
}

fn sent_saves(sent: &mut Sent) -> Vec<i64> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Admin(Admin::SaveHistory(keep)) => Some(keep),
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
fn a_lower_message_limit_asks_before_deleting() {
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
            let default = MessageHistory {
                keep: 10_000,
                ceiling: None,
            };
            let (mut h, mut sent) = screen(size, dark, default);
            assert!(!save_enabled(&h));
            shot(&mut h, "history");

            // Keeping everything deletes nothing, so it saves straight away.
            click(&mut h, Role::Switch, t!("admin.history_keep_all"));
            assert!(save_enabled(&h));
            sent_saves(&mut sent);
            click(&mut h, Role::Button, t!("common.save"));
            assert_eq!(sent_saves(&mut sent), [0]);
            let home = &mut h.state_mut().as_mut().unwrap().0;
            let settings = home.server_settings.as_mut().unwrap();
            settings.apply(AdminUpdate::History(Some(MessageHistory {
                keep: 0,
                ..default
            })));
            h.run_steps(4);
            assert!(!save_enabled(&h));

            // Going back to a limit deletes older messages: only after confirming.
            click(&mut h, Role::Switch, t!("admin.history_keep_all"));
            click(&mut h, Role::Button, t!("common.save"));
            assert!(sent_saves(&mut sent).is_empty());
            let delete = by()
                .role(Role::Button)
                .label(t!("admin.history_confirm_delete"));
            assert!(h.query(delete).is_some());
            shot(&mut h, "history-confirm");
            click(&mut h, Role::Button, t!("admin.history_confirm_delete"));
            assert_eq!(sent_saves(&mut sent), [10_000]);

            // A host cap: no "Keep all", and the help names the setting that raises it.
            let (mut h, _) = screen(
                size,
                dark,
                MessageHistory {
                    keep: 5_000,
                    ceiling: Some(5_000),
                },
            );
            assert!(h.query(keep_all()).is_none());
            let cap = t!(
                "admin.history_host_limit",
                limit = 5_000,
                setting = "RUSTTALK_MESSAGE_RETAIN"
            );
            assert!(h.query_by_label_contains(&cap).is_some());
            shot(&mut h, "history-host-limit");
        }
    }
}
