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
