use super::*;
use crate::{
    admin,
    net::{Admin, AdminUpdate, AuditEntry},
};
use egui_kittest::{Harness, kittest::Queryable};
use std::path::PathBuf;

/// The `before` of each audit log page asked for since last time.
fn audit_pages(sent: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<Option<i64>> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Admin(Admin::Audit { before, .. }) => Some(before),
            _ => None,
        })
        .collect()
}

/// Settings open on Members without touching the audit log; its tab asks for the newest
/// page, and "Show older" for the page after the last one shown.
#[test]
fn audit_log_loads_only_on_its_tab_a_page_at_a_time() {
    for dark in [true, false] {
        for size in [Vec2::new(390., 844.), Vec2::new(1100., 700.)] {
            let (mut base, mut sent) = files_harness(dark);
            let mut home = base.state_mut().take().unwrap();
            let perms = &mut home.servers[0].dir.perms;
            (perms.audit, perms.ban, perms.kick) = (true, true, true);
            admin::open(&mut home, 0, admin::Tab::Members);
            let mut h = Harness::builder()
                .with_size(size)
                .with_context_setup(ui::install)
                .build_ui_state(|ui, home: &mut Home| admin::window(ui.ctx(), home), home);
            h.ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            h.run_steps(4);
            assert_eq!(audit_pages(&mut sent), []);

            // Phones may show the page list first, or Members with Back to it.
            if let Some(back) = h.query_by_label(t!("common.back")) {
                back.click();
                h.run_steps(2);
            }
            h.get_by_label_contains(t!("admin.audit_log")).click();
            h.run_steps(2);
            assert_eq!(audit_pages(&mut sent), [None]);

            // A full page of 50: 120 down to 71.
            let entries = (71..=120)
                .rev()
                .map(|at| AuditEntry {
                    target_id: None,
                    actor: ANN,
                    action: if at % 3 == 0 {
                        "member_banned"
                    } else {
                        "joined"
                    }
                    .into(),
                    target: (at % 3 == 0).then_some(ME),
                    at: 1_700_000_000 + at * 3600,
                    details: Default::default(),
                })
                .collect();
            let settings = h.state_mut().server_settings.as_mut().unwrap();
            settings.apply(AdminUpdate::Audit {
                entries,
                older: false,
                next: Some(71),
            });
            h.run_steps(2);
            let older = h.get_by_label(t!("admin.show_older"));
            older.scroll_to_me();
            // Long enough for the scroll to settle.
            h.run_steps(30);
            if let Some(dir) = std::env::var_os("RUSTTALK_AUDIT_SCREENSHOTS") {
                let theme = if dark { "dark" } else { "light" };
                h.render()
                    .unwrap()
                    .save(PathBuf::from(dir).join(format!(
                        "audit-{theme}-{}x{}.png",
                        size.x as u32, size.y as u32
                    )))
                    .unwrap();
            }
            h.get_by_label(t!("admin.show_older")).click();
            h.run_steps(2);
            assert_eq!(audit_pages(&mut sent), [Some(71)]);
            // Gone until that page arrives, so it can't be asked for twice.
            assert!(h.query_by_label(t!("admin.show_older")).is_none());
        }
    }
}

/// Entries with details name the role, channel and what changed; older entries keep
/// their plain sentence.
#[test]
fn audit_log_shows_what_changed() {
    for dark in [true, false] {
        for size in [Vec2::new(390., 844.), Vec2::new(1100., 700.)] {
            let (mut base, _sent) = files_harness(dark);
            let mut home = base.state_mut().take().unwrap();
            let server = &mut home.servers[0];
            server.dir.perms.audit = true;
            server.dir.roles = vec![crate::server::Role {
                id: "member".into(),
                name: "Member".into(),
                name_known: true,
                rank: 1,
                permissions: Vec::new(),
                custom: false,
                color: None,
                separate: false,
            }];
            admin::open(&mut home, 0, admin::Tab::Audit);
            let mut h = Harness::builder()
                .with_size(size)
                .with_context_setup(ui::install)
                .build_ui_state(|ui, home: &mut Home| admin::window(ui.ctx(), home), home);
            h.ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            h.run_steps(2);
            // Phones show the page list first.
            if size.x < 500. {
                h.get_by_label_contains(t!("admin.audit_log")).click();
                h.run_steps(2);
            }
            let entry = |action: &str, target, target_id, details| AuditEntry {
                target_id,
                actor: ANN,
                action: action.into(),
                target,
                at: 1_700_000_000,
                details,
            };
            let limits = |max_file_bytes: i64| {
                serde_json::json!({
                    "max_file_bytes": max_file_bytes,
                    "member_storage_bytes": 1,
                    "server_storage_bytes": 1,
                    "retention_secs": 86_400,
                    "max_stickers": 50,
                })
            };
            let entries = vec![
                entry(
                    "channel_permissions_changed",
                    None,
                    Some(PROJECTS),
                    serde_json::json!({ "role_id": "member", "before": 3, "after": 1 }),
                ),
                entry(
                    "member_roles_changed",
                    Some(ME),
                    None,
                    serde_json::json!({ "added": ["member"], "removed": ["gone"] }),
                ),
                entry(
                    "server_updated",
                    None,
                    None,
                    serde_json::json!({ "changed": ["name", "icon"] }),
                ),
                entry(
                    "file_limits_changed",
                    None,
                    None,
                    serde_json::json!({ "before": limits(32 << 20), "after": limits(1) }),
                ),
                entry(
                    "listing_changed",
                    None,
                    None,
                    serde_json::json!({ "enabled": true }),
                ),
                // From a server before details.
                entry(
                    "channel_permissions_changed",
                    None,
                    Some(PROJECTS),
                    serde_json::Value::Null,
                ),
            ];
            let settings = h.state_mut().server_settings.as_mut().unwrap();
            settings.apply(AdminUpdate::Audit {
                entries,
                older: false,
                next: None,
            });
            h.run_steps(4);
            if let Some(dir) = std::env::var_os("RUSTTALK_AUDIT_SCREENSHOTS") {
                let theme = if dark { "dark" } else { "light" };
                h.render()
                    .unwrap()
                    .save(PathBuf::from(dir).join(format!(
                        "audit-details-{theme}-{}x{}.png",
                        size.x as u32, size.y as u32
                    )))
                    .unwrap();
            }
            for text in [
                t!(
                    "audit.channel_role_permissions",
                    actor = "Ann",
                    role = "Member",
                    channel = "#projects"
                ),
                format!("− {}", t!("admin.flag_write")),
                format!("+ Member, − {}", t!("audit.unknown_role")),
                format!("{}, {}", t!("home.server_name"), t!("audit.part_icon")),
                t!(
                    "audit.changed_value",
                    label = t!("admin.max_file_size"),
                    before = crate::files::size(32 << 20),
                    after = crate::files::size(1)
                ),
                t!("audit.listed", actor = "Ann"),
                t!("audit.channel_permissions_changed", actor = "Ann"),
            ] {
                assert!(h.query_by_label(&text).is_some(), "{text}");
            }
        }
    }
}
