//! Server settings > Files: only a ceiling the host set is called the host's limit.

use super::*;
use crate::{admin, net::AdminUpdate};
use egui_kittest::{
    Harness,
    kittest::{Queryable, by},
};
use rusttalk_core::auth::{FileLimits, FileSettings};

const DEFAULTS: FileLimits = FileLimits {
    max_file_bytes: 32 * 1024 * 1024,
    member_storage_bytes: 500 * 1024 * 1024,
    server_storage_bytes: 5 * 1024 * 1024 * 1024,
    retention_secs: 30 * 86_400,
    max_stickers: 50,
};

fn screen(size: Vec2, dark: bool, ceilings: FileLimits) -> Harness<'static, Home> {
    let (mut base, _sent) = files_harness(dark);
    let mut home = base.state_mut().take().unwrap();
    home.servers[0].dir.perms.manage_server = true;
    admin::open(&mut home, 0, admin::Tab::Files);
    home.server_settings
        .as_mut()
        .unwrap()
        .apply(AdminUpdate::Files(FileSettings {
            limits: DEFAULTS,
            ceilings,
            used_bytes: 3 * 1024 * 1024,
        }));
    let mut h = Harness::builder()
        .with_size(size)
        .with_context_setup(ui::install)
        .with_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        })
        .build_ui_state(|ui, home: &mut Home| admin::window(ui.ctx(), home), home);
    h.run_steps(4);
    // Phones may start on the page list.
    let field = by().role(Role::TextInput).label(t!("admin.max_file_size"));
    if h.query(field).is_none() {
        h.get_by_label(t!("admin.files")).click();
        h.run_steps(4);
    }
    h
}

/// Add `text` after what a limit's field shows.
fn append(h: &mut Harness<'static, Home>, label: &str, text: &str) {
    h.get(by().role(Role::TextInput).label(label)).focus();
    h.run_steps(2);
    h.get(by().role(Role::TextInput).label(label))
        .type_text(text);
    h.run_steps(4);
}

fn mb(amount: &str) -> String {
    t!(
        "admin.limit_amount",
        amount = amount,
        unit = t!("admin.limit_mb")
    )
}

#[test]
fn file_limits_explain_only_a_ceiling_the_host_set() {
    let screenshots = std::env::var("RUSTTALK_TEST_SCREENSHOTS").ok();
    for dark in [true, false] {
        for size in [Vec2::new(390., 844.), Vec2::new(1100., 700.)] {
            let theme = if dark { "dark" } else { "light" };
            let shot = |h: &mut Harness<'static, Home>, name: &str| {
                if let Some(dir) = &screenshots {
                    h.render()
                        .unwrap()
                        .save(format!("{dir}/{name}-{}x{}-{theme}.png", size.x, size.y))
                        .unwrap();
                }
            };

            // No host ceiling: storage may go far past the old defaults, and the
            // file size names RustTalk's own limit instead of the host's.
            let mut h = screen(size, dark, FileLimits::MAX);
            let member_help = t!("admin.host_ceiling", limit = mb("500"));
            assert!(h.query_by_label_contains(&member_help).is_none());
            append(&mut h, t!("admin.member_storage"), "0000");
            append(&mut h, t!("admin.max_file_size"), "0");
            assert!(
                h.query_by_label(&t!("admin.limit_above_app", limit = mb("32")))
                    .is_some()
            );
            assert!(h.query_by_label_contains(&member_help).is_none());
            shot(&mut h, "files-no-host-limit");

            // The host's ceiling: said plainly, with the setting that raises it.
            let mut h = screen(size, dark, DEFAULTS);
            assert!(h.query_by_label_contains(&member_help).is_some());
            append(&mut h, t!("admin.member_storage"), "0");
            let above = t!(
                "admin.limit_above_host",
                limit = mb("500"),
                setting = "RUSTTALK_FILE_MEMBER_BYTES"
            );
            assert!(h.query_by_label(&above).is_some());
            shot(&mut h, "files-host-limit");
        }
    }
}
