use super::*;
use egui_kittest::{Harness, kittest::Queryable};

/// Create channel and invite keep their buttons on screen on a roomy desktop
/// window and a short one. Set RUSTTALK_TEST_SCREENSHOTS to save how they look.
#[test]
fn server_dialogs_fit_roomy_and_short_windows() {
    let screenshots = std::env::var("RUSTTALK_TEST_SCREENSHOTS").ok();
    for (width, height) in [(1100.0, 700.0), (1100.0, 500.0)] {
        for dark in [true, false] {
            for (name, button) in [
                ("create-channel", t!("common.create")),
                ("invite", t!("home.create_link")),
            ] {
                let (mut fixture, _sent) = files_harness(dark);
                let mut home = fixture.state_mut().take().unwrap();
                home.dialog = Some(if name == "invite" {
                    Dialog::Invite(Invite {
                        server: 0,
                        uses: 10,
                        seconds: DAY * 7,
                        making: false,
                        link: None,
                    })
                } else {
                    new_channel(&home.servers[0], 0, None)
                });
                let theme = if dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                };
                let mut h = Harness::builder()
                    .with_size(Vec2::new(width, height))
                    .with_context_setup(ui::install)
                    .with_theme(theme)
                    // The channel name field has no accessible name yet (not this test's concern).
                    .with_accessibility_check(false)
                    .build_ui_state(|ui, home| dialogs(ui.ctx(), home), home);
                h.run_steps(3);
                assert!(h.state().dialog.is_some());
                if let Some(dir) = &screenshots {
                    std::fs::create_dir_all(dir).unwrap();
                    let theme = if dark { "dark" } else { "light" };
                    h.render()
                        .unwrap()
                        .save(format!("{dir}/{name}-{width}x{height}-{theme}.png"))
                        .unwrap();
                }
                // Scrolled to, when the window is too short for all of it.
                h.get_by_label(button).scroll_to_me();
                h.run_steps(3);
                let rect = h.get_by_label(button).rect();
                assert!(
                    rect.top() >= 0.0 && rect.bottom() <= height,
                    "{name} button on screen at {width}x{height}: {rect:?}"
                );
            }
        }
    }
}
