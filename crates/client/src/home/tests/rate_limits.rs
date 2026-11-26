//! Server settings > Rate limits: picking a preset saves it, a host cap rules out looser
//! ones; and a message the server slowed down shows a calm note above the composer.

use super::*;
use crate::net::{Admin, AdminUpdate};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable, by},
};
use rusttalk_core::auth::{RateLimitPreset, RateLimits};

type Sent = tokio::sync::mpsc::UnboundedReceiver<Command>;

/// The open server; with `saved`, its Rate limits page.
fn screen(
    size: Vec2,
    dark: bool,
    saved: Option<RateLimits>,
) -> (Harness<'static, Option<Screen>>, Sent) {
    let (mut files, sent) = files_harness(dark);
    let mut home = files.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.servers[0].open = Some(PROJECTS);
    home.find.opened = false;
    home.servers[0].dir.perms.manage_server = true;
    if let Some(saved) = saved {
        crate::admin::open(&mut home, 0, crate::admin::Tab::RateLimits);
        let settings = home.server_settings.as_mut().unwrap();
        settings.apply(AdminUpdate::RateLimits(Some(saved)));
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
    if saved.is_some() && harness.query(card(t!("admin.rate_strict"))).is_none() {
        // Phones may start on the page list.
        harness.get_by_label(t!("admin.rate_limits")).click();
        harness.run_steps(4);
    }
    (harness, sent)
}

fn card(label: &str) -> egui_kittest::kittest::By<'_> {
    by().role(Role::RadioButton).label(label)
}

fn sent_saves(sent: &mut Sent) -> Vec<RateLimitPreset> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Admin(Admin::SaveRateLimits(preset)) => Some(preset),
            _ => None,
        })
        .collect()
}

fn enabled(h: &Harness<'_, Option<Screen>>, by: egui_kittest::kittest::By<'_>) -> bool {
    !h.get(by).accesskit_node().is_disabled()
}

#[test]
fn owners_pick_a_preset_within_the_host_cap_and_slowed_messages_say_so() {
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
            let save = || by().role(Role::Button).label(t!("common.save"));
            let standard = RateLimits {
                preset: RateLimitPreset::Standard,
                ceiling: None,
            };
            let (mut h, mut sent) = screen(size, dark, Some(standard));
            assert!(!enabled(&h, save()));
            shot(&mut h, "rate-limits");

            h.get(card(t!("admin.rate_strict"))).click_accesskit();
            h.run_steps(4);
            assert!(enabled(&h, save()));
            sent_saves(&mut sent);
            h.get(save()).click();
            h.run_steps(4);
            assert_eq!(sent_saves(&mut sent), [RateLimitPreset::Strict]);

            // A host cap at Standard: Relaxed can't be picked, and the note names the setting.
            let capped = RateLimits {
                preset: RateLimitPreset::Standard,
                ceiling: Some(RateLimitPreset::Standard),
            };
            let (mut h, _) = screen(size, dark, Some(capped));
            assert!(!enabled(&h, card(t!("admin.rate_relaxed"))));
            assert!(enabled(&h, card(t!("admin.rate_strict"))));
            let cap = t!(
                "admin.rate_host_limit",
                preset = t!("admin.rate_standard"),
                setting = "RUSTTALK_RATE_LIMITS"
            );
            assert!(h.query_by_label_contains(&cap).is_some());
            shot(&mut h, "rate-limits-host-cap");

            // A message waiting after a 429: a note above the composer, until it went.
            let (mut h, _) = files_harness(dark);
            h.ctx.data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
            h.set_size(size);
            let address = {
                let home = h.state_mut().as_mut().unwrap();
                home.servers[0].chats.get_mut(&PROJECTS).unwrap().draft =
                    "And one more thing".into();
                home.servers[0].address.clone()
            };
            h.run_steps(3);
            let note = t!("home.slow_down");
            assert!(h.query_by_label(note).is_none());
            let slow = |h: &mut Harness<'static, Option<Home>>, waiting| {
                let ctx = h.ctx.clone();
                let home = h.state_mut().as_mut().unwrap();
                let update = crate::net::Update::SlowDown(address.clone(), PROJECTS, waiting);
                home.apply(&ctx, update, 0.0);
                h.run_steps(4);
            };
            slow(&mut h, true);
            assert!(h.query_by_label(note).is_some());
            if let Some(dir) = &screenshots {
                let name = format!("{dir}/slow-down-{}x{}-{theme}.png", size.x, size.y);
                h.render().unwrap().save(name).unwrap();
            }
            slow(&mut h, false);
            assert!(h.query_by_label(note).is_none());
        }
    }
}
