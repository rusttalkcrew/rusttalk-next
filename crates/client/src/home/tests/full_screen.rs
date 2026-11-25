//! Full screen on a stream keeps the call's and the stream's controls on hover, and
//! leaves with Escape, its hover button or Back (#573).

use super::*;
use crate::server::{Call, CallState, Peer, Screen as Shared, Stats};
use egui_kittest::{Harness, kittest::Queryable};

type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);

/// In a call with Ann, watching her screen in full screen.
fn full_screen(dark: bool, size: Vec2, data: &std::path::Path) -> Harness<'static, Option<Screen>> {
    let (mut base, _sent) = files_harness(dark);
    let mut home = base.state_mut().take().unwrap();
    home.view = View::Server(0);
    home.layout.list = false;
    home.servers[0].open = Some(PROJECTS);
    home.servers[0].dir.categories[0].channels[0].voice = true;
    home.find.opened = false;
    home.voice.call = Some(Call {
        server: home.servers[0].address.clone(),
        channel: PROJECTS,
        state: CallState::Connected,
        stats: Stats::default(),
        can_talk: true,
        can_share_video: true,
    });
    let peer = |person, screens| Peer {
        person,
        connecting: false,
        unknown: false,
        status: Default::default(),
        apps: vec![],
        camera: None,
        terminal: None,
        diagram: None,
        screens,
    };
    let screen = Shared {
        sid: "remote".into(),
        name: "Display".into(),
        viewers: 1,
        local: false,
        control: None,
    };
    home.servers[0]
        .voice
        .insert(PROJECTS, vec![peer(ME, vec![]), peer(ANN, vec![screen])]);
    let mut prefs = home.notify.prefs().clone();
    prefs.desktop = false;
    home.notify.set_prefs(prefs);
    let mut settings = home.voice.audio.settings();
    settings.set_appearance(ui::Appearance {
        mode: if dark {
            ui::Mode::Dark
        } else {
            ui::Mode::Light
        },
        ..Default::default()
    });
    home.voice.audio.set_settings(settings);
    let mut h = Harness::builder().with_size(size).build_ui_state(
        |ui, screen: &mut Option<Screen>| {
            if let Some((home, updates, keys)) = screen {
                show(ui, home, updates, keys);
            }
        },
        None,
    );
    ui::install(&h.ctx);
    *h.state_mut() = Some((
        home,
        crate::update::Updater::new(data),
        crate::keystore::without_keychain(data),
    ));
    h.run_steps(4);
    h.get_by_label("Watch Ann").click();
    h.run_steps(2);
    // A picture to show: a soft gradient, like a desktop.
    let (w, h_px) = (320, 180);
    let pixels = (0..w * h_px)
        .map(|at| {
            let (x, y) = (at % w, at / w);
            egui::Color32::from_rgb(40 + (x * 120 / w) as u8, 60, 90 + (y * 120 / h_px) as u8)
        })
        .collect();
    let image = egui::ColorImage::new([w, h_px], pixels);
    home_mut(&mut h).voice.video.put("remote", image);
    home_mut(&mut h)
        .voice
        .stage
        .set_presenting_and_full_screen("remote");
    at(&mut h, 0.0);
    h
}

fn home<'a>(h: &'a Harness<'_, Option<Screen>>) -> &'a Home {
    &h.state().as_ref().unwrap().0
}

fn home_mut<'a>(h: &'a mut Harness<'_, Option<Screen>>) -> &'a mut Home {
    &mut h.state_mut().as_mut().unwrap().0
}

/// Two frames at `seconds` past the start.
fn at(h: &mut Harness<'_, Option<Screen>>, seconds: f64) {
    for _ in 0..2 {
        h.input_mut().time = Some(100.0 + seconds);
        h.step();
    }
}

fn shown(h: &Harness<'_, Option<Screen>>) -> bool {
    h.query_by_label(t!("call.exit_full_screen")).is_some()
}

fn capture(h: &mut Harness<'_, Option<Screen>>, size: Vec2, dark: bool, state: &str) {
    if let Ok(dir) = std::env::var("RUSTTALK_FULL_SCREEN_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let name = format!("{dir}/{}x{}-{theme}-{state}.png", size.x, size.y);
        h.render().unwrap().save(name).unwrap();
    }
}

#[test]
fn full_screen_controls_show_on_movement_and_hide_when_idle() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)] {
            let mut h = full_screen(dark, size, data.path());
            assert!(home(&h).voice.stage.full.is_some());
            // On entering: the way out, the stream's own buttons and the call's.
            assert!(shown(&h));
            let labels = [
                t!("call.stop_watching"),
                t!("call.stream_options"),
                t!("call.mute"),
                t!("call.leave"),
            ];
            for label in labels {
                let button = h.get_by_label(label).rect();
                assert!(button.height() >= ui::TOUCH_TARGET, "{label}");
                assert!(
                    button.right() <= size.x && button.bottom() <= size.y,
                    "{label}"
                );
            }
            assert!(h.query_by_label(t!("call.camera_on")).is_some());
            // The stream's menu has its stats, and no layout rows.
            h.get_by_label(t!("call.stream_options")).click();
            at(&mut h, 0.5);
            assert!(h.query_by_label(t!("call.stats_for_nerds")).is_some());
            assert!(h.query_by_label(t!("call.focus")).is_none());
            capture(&mut h, size, dark, "menu");
            // Escape closes the menu, and only the menu.
            h.key_press(egui::Key::Escape);
            at(&mut h, 0.6);
            assert!(h.query_by_label(t!("call.stats_for_nerds")).is_none());
            assert!(home(&h).voice.stage.full.is_some());
            h.hover_at(egui::pos2(size.x / 2.0, size.y / 2.0));
            at(&mut h, 0.7);
            capture(&mut h, size, dark, "controls");

            // Idle: gone, and nothing covers the picture.
            at(&mut h, 0.7 + 3.0);
            assert!(!shown(&h));
            assert!(h.query_by_label(t!("call.mute")).is_none());
            capture(&mut h, size, dark, "idle");
            // The pointer moves: back again.
            h.hover_at(egui::pos2(size.x / 3.0, size.y / 2.0));
            at(&mut h, 4.0);
            assert!(shown(&h));
            // Resting on the controls keeps them.
            let mute = h.get_by_label(t!("call.mute")).rect().center();
            h.hover_at(mute);
            at(&mut h, 4.1);
            at(&mut h, 20.0);
            assert!(shown(&h));

            // Escape leaves full screen, and the call goes on.
            h.key_press(egui::Key::Escape);
            at(&mut h, 20.1);
            assert!(home(&h).voice.stage.full.is_none());
            assert!(home(&h).voice.call.is_some());
            assert!(home(&h).voice.watching.is_some());
        }
    }
}

#[test]
fn the_hover_button_and_back_leave_full_screen() {
    let data = tempfile::tempdir().unwrap();
    let size = Vec2::new(1100.0, 700.0);
    let mut h = full_screen(true, size, data.path());
    h.get_by_label(t!("call.exit_full_screen")).click();
    at(&mut h, 0.5);
    assert!(home(&h).voice.stage.full.is_none());
    // Back (Android) leaves full screen before anything else.
    home_mut(&mut h)
        .voice
        .stage
        .set_presenting_and_full_screen("remote");
    at(&mut h, 1.0);
    assert!(home(&h).can_go_back());
    assert!(home_mut(&mut h).go_back());
    assert!(home(&h).voice.stage.full.is_none());
    // Stop watching leaves too: nothing would be left to show.
    home_mut(&mut h)
        .voice
        .stage
        .set_presenting_and_full_screen("remote");
    at(&mut h, 2.0);
    h.get_by_label(t!("call.stop_watching")).click();
    at(&mut h, 2.5);
    assert!(home(&h).voice.stage.full.is_none());
    assert!(home(&h).voice.watching.is_none());
}
