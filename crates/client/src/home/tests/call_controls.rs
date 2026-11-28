//! Phones get the call's controls in the call view, with the chat hidden at first
//! (#513); tablets and desktops keep them in the sidebar's voice panel.

use super::*;
use crate::server::{Call, CallState, Peer, Stats};
use egui_kittest::{Harness, kittest::Queryable};

type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);

fn in_call(dark: bool, size: Vec2, data: &std::path::Path) -> Harness<'static, Option<Screen>> {
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
    let peer = |person| Peer {
        person,
        connecting: false,
        unknown: false,
        status: Default::default(),
        apps: vec![],
        camera: None,
        terminal: None,
        diagram: None,
        screens: vec![],
    };
    home.servers[0]
        .voice
        .insert(PROJECTS, vec![peer(ME), peer(ANN)]);
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
    h
}

fn home<'a>(h: &'a Harness<'_, Option<Screen>>) -> &'a Home {
    &h.state().as_ref().unwrap().0
}

fn capture(h: &mut Harness<'_, Option<Screen>>, size: Vec2, dark: bool, state: &str) {
    if let Ok(dir) = std::env::var("RUSTTALK_CALL_CONTROLS_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let name = format!("{dir}/{}x{}-{theme}-{state}.png", size.x, size.y);
        h.render().unwrap().save(name).unwrap();
    }
}

#[test]
fn shared_apps_show_under_their_sharer_with_bars_and_volume() {
    let data = tempfile::tempdir().unwrap();
    for (size, dark) in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let mut h = in_call(dark, size, data.path());
        let home = &mut h.state_mut().as_mut().unwrap().0;
        home.layout.list = true;
        let sid = |s: &str| s.to_owned().try_into().unwrap();
        home.servers[0].voice.get_mut(&PROJECTS).unwrap()[1].apps = vec![
            (sid("TR_a"), "Spotify".into()),
            (sid("TR_b"), "YouTube Music".into()),
        ];
        let audio = &home.voice.audio;
        audio.set_listening("TR_a", true);
        audio.play_app_for_test("TR_a", (ANN, Some("Spotify".into())), -18.0);
        h.run_steps(4);
        let label = |name| format!("{}: {name}", t!("call.listen"));
        let row = h.get_by_label(&label("Spotify")).rect();
        assert!(row.right() <= size.x, "fits the window");
        assert!(h.query_by_label(&label("YouTube Music")).is_some());
        assert!(
            h.query_by_label(&format!("{}: Spotify", t!("call.volume")))
                .is_some()
        );
        capture(&mut h, size, dark, "shared-apps");
    }
}

#[test]
fn phones_show_call_controls_and_hide_the_chat_at_first() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        let size = Vec2::new(390.0, 844.0);
        let mut h = in_call(dark, size, data.path());
        assert!(home(&h).layout.phone);
        // One mic and one leave button: the call view's, the sidebar is hidden.
        let mute = h.get_by_label(t!("call.mute")).rect();
        let leave = h.get_by_label(t!("call.leave")).rect();
        assert!(mute.height() >= ui::TOUCH_TARGET && leave.height() >= ui::TOUCH_TARGET);
        assert!(leave.bottom() > size.y - 40.0 && leave.right() <= size.x);
        assert!((mute.center().y - leave.center().y).abs() < 1.0, "one row");
        assert!(h.query_by_label(t!("call.deafen")).is_some());
        // The chat starts hidden; the desktop choice is left alone.
        assert!(h.query_by_label(t!("layout.show_chat")).is_some());
        assert!(home(&h).layout.call_split().chat_hidden);
        assert!(!home(&h).layout.split.chat_hidden);
        capture(&mut h, size, dark, "call");

        h.get_by_label(t!("call.mute")).click();
        h.run_steps(2);
        assert!(home(&h).voice.audio.muted());
        assert!(h.query_by_label(t!("call.unmute")).is_some());

        h.get_by_label(t!("layout.show_chat")).click();
        h.run_steps(3);
        assert!(!home(&h).layout.call_split().chat_hidden);
        assert!(h.query_by_label(t!("layout.hide_chat")).is_some());
        // The controls stay below the call and its chat.
        let leave = h.get_by_label(t!("call.leave")).rect();
        assert!(leave.bottom() > size.y - 40.0);
        capture(&mut h, size, dark, "chat");

        h.get_by_label(t!("call.leave")).click();
        h.run_steps(2);
        assert!(home(&h).voice.call.is_none());
    }
}

#[test]
fn tablets_and_desktops_keep_controls_in_the_sidebar() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(820.0, 1180.0), Vec2::new(1100.0, 700.0)] {
            let mut h = in_call(dark, size, data.path());
            assert!(!home(&h).layout.phone);
            // The chat shows beside the call as before, and the only leave button is
            // the voice panel's in the sidebar.
            assert!(!home(&h).layout.call_split().chat_hidden);
            assert!(h.query_by_label(t!("layout.hide_chat")).is_some());
            let leave = h.get_by_label(t!("call.leave")).rect();
            let sidebar = RAIL_WIDTH + home(&h).layout.channels;
            assert!(leave.right() <= sidebar, "{size:?}: {leave:?}");
            assert!(leave.height() < ui::TOUCH_TARGET);
            capture(&mut h, size, dark, "call");
            // A right-click on mic, sound or camera lists the devices to pick from.
            for (button, item) in [
                (t!("call.mute"), t!("common.system_default")),
                (t!("call.deafen"), t!("common.system_default")),
                (t!("call.camera_on"), t!("camera.first")),
                (t!("call.share_screen"), "1440p"),
            ] {
                h.get_by_label(button).click_secondary();
                h.run_steps(2);
                assert!(h.query_by_label(item).is_some(), "{button}");
                capture(&mut h, size, dark, &format!("right-click-{button}"));
                h.key_press(egui::Key::Escape);
                h.run_steps(2);
                assert!(h.query_by_label(item).is_none(), "{button}");
            }
        }
    }
}

/// `item`, in a menu just opened from `button`, sits right above the button and lines
/// up with its left or right edge, or its middle where neither fits, inside the window.
fn above(h: &Harness<'_, Option<Screen>>, button: egui::Rect, item: &str, size: Vec2) {
    let rect = h.get_by_label(item).rect();
    assert!(
        rect.bottom() <= button.top(),
        "{item}: {rect:?} over {button:?}"
    );
    let lined_up = (rect.left() - button.left()).abs() < 16.0
        || (rect.right() - button.right()).abs() < 16.0
        || (rect.center().x - button.center().x).abs() < 16.0;
    assert!(lined_up, "{item}: {rect:?} at {button:?}");
    assert!(rect.min.x >= 0.0 && rect.max.x <= size.x && rect.min.y >= 0.0);
}

/// Right-click menus of the call's buttons and the gear open right above their button,
/// like Discord's; the gear's lists the Settings pages.
#[test]
fn call_bar_menus_open_at_their_buttons() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(820.0, 1180.0), Vec2::new(1100.0, 700.0)] {
            let mut h = in_call(dark, size, data.path());
            for (button, item) in [
                (t!("call.mute"), t!("common.system_default")),
                (t!("call.deafen"), t!("common.system_default")),
                (t!("call.camera_on"), t!("camera.first")),
                (t!("call.share_screen"), t!("call.show_cursor")),
            ] {
                let rect = h.get_by_label(button).rect();
                h.get_by_label(button).click_secondary();
                h.run_steps(3);
                above(&h, rect, item, size);
                h.key_press(egui::Key::Escape);
                h.run_steps(2);
            }
            let more = h.get_by_label(t!("call.more")).rect();
            h.get_by_label(t!("call.more")).click();
            h.run_steps(3);
            above(&h, more, t!("diagram.start"), size);
            h.key_press(egui::Key::Escape);
            h.run_steps(2);

            // The gear: a click still opens Settings, a right-click its pages.
            let gear = h.get_by_label(t!("settings.title")).rect();
            h.get_by_label(t!("settings.title")).click_secondary();
            h.run_steps(3);
            for page in [
                t!("settings.my_account"),
                t!("settings.appearance"),
                t!("settings.about"),
            ] {
                assert!(h.query_by_label(page).is_some(), "{page}");
            }
            above(&h, gear, t!("settings.about"), size);
            capture(&mut h, size, dark, "gear-menu");
            h.get_by_label(t!("settings.voice")).click();
            h.run_steps(2);
            assert!(home(&h).settings == Some(Tab::Voice));
        }
    }
}

/// While sharing, the screen button's menu changes the running share's quality: it
/// starts again with the new choice, which is kept for later shares.
#[test]
fn share_settings_change_while_sharing() {
    use crate::call::Sharing;
    let Some(target) = crate::screen::targets()
        .ok()
        .and_then(|t| t.into_iter().next())
    else {
        return;
    };
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let mut h = in_call(dark, size, data.path());
            let voice = &mut h.state_mut().as_mut().unwrap().0.voice;
            let options = crate::screen::Options::default();
            crate::call::note_shared_screen(voice, &target, options, true);
            voice.sharing = Sharing::On;
            h.run_steps(3);
            let button = h.get_by_label(t!("call.stop_sharing")).rect();
            h.get_by_label(t!("call.stop_sharing")).click_secondary();
            h.run_steps(3);
            for item in [
                t!("call.change_my_share"),
                t!("call.quality"),
                t!("call.frame_rate"),
                t!("call.show_cursor"),
                t!("call.stream_sound"),
            ] {
                above(&h, button, item, size);
            }
            h.get_by_label(t!("call.quality")).hover();
            h.run_steps(3);
            capture(&mut h, size, dark, "share-menu");
            h.get_by_label("720p").click();
            h.run_steps(3);
            let voice = &home(&h).voice;
            assert!(voice.sharing == Sharing::Starting, "started again");
            assert_eq!(voice.screen_options().max_height, Some(720));
            assert_eq!(voice.audio.settings().screen_height, Some(720));
            assert!(h.query_by_label(t!("call.change_my_share")).is_none());
        }
    }
}

/// Our camera on, as a phone's camera that can do what `controls` says.
fn camera_on(h: &mut Harness<'_, Option<Screen>>, controls: crate::camera::Controls) {
    let ctx = h.ctx.clone();
    let home = &mut h.state_mut().as_mut().unwrap().0;
    let me = home.servers[0].dir.me;
    for peer in home.servers[0].voice.get_mut(&PROJECTS).unwrap() {
        if peer.person == me {
            peer.camera = Some("my camera".into());
        }
    }
    // A stand-in picture, lighter on the left so the mirrored self view shows.
    let mut pixels = egui::ColorImage::filled([160, 120], egui::Color32::from_rgb(40, 60, 90));
    for y in 0..120 {
        for x in 0..60 {
            pixels[(x, y)] = egui::Color32::from_rgb(200, 170, 120);
        }
    }
    let camera = &mut home.voice.camera;
    camera.fake_picture(ctx.load_texture("fake camera", pixels, Default::default()));
    camera.fake = Some(controls);
    camera.on = true;
    h.run_steps(3);
}

/// On phones and tablets a tap on the camera button, while it is on, opens a menu with
/// what the camera can do; the phone's call view and the tablet's sidebar alike.
#[test]
fn camera_button_opens_a_menu_while_on() {
    use crate::camera::{Controls, Pick};
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(820.0, 1180.0)] {
            let mut h = in_call(dark, size, data.path());
            let back = Controls {
                flip: true,
                torch: Some(false),
                busy: false,
            };
            camera_on(&mut h, back);
            // Our own picture is only on the stage, never in a sidebar preview.
            assert!(h.query_by_label(t!("camera.preview")).is_none());
            assert!(h.query_by_label(t!("call.camera_off")).is_none());
            let button = h.get_by_label(t!("camera.camera")).rect();
            if size.x < 400.0 {
                assert!(button.bottom() > size.y - 40.0, "the call view's bar");
            } else {
                let sidebar = RAIL_WIDTH + home(&h).layout.channels;
                assert!(button.right() <= sidebar, "the sidebar's card");
            }
            capture(&mut h, size, dark, "camera");

            h.get_by_label(t!("camera.camera")).click();
            h.run_steps(3);
            assert!(h.query_by_label(t!("camera.flip")).is_some());
            assert!(h.query_by_label(t!("camera.torch_on")).is_some());
            assert!(h.query_by_label(t!("call.camera_off")).is_some());
            for item in [t!("camera.flip"), t!("call.camera_off")] {
                let rect = h.get_by_label(item).rect();
                assert!(
                    rect.min.x >= 0.0 && rect.max.x <= size.x,
                    "{item}: {rect:?}"
                );
                assert!(
                    rect.min.y >= 0.0 && rect.max.y <= size.y,
                    "{item}: {rect:?}"
                );
            }
            capture(&mut h, size, dark, "camera-menu");
            h.get_by_label(t!("camera.torch_on")).click();
            h.run_steps(3);
            assert_eq!(home(&h).voice.camera.test_picks, [Pick::Torch(true)]);
            assert!(home(&h).voice.camera.on);
            assert!(h.query_by_label(t!("camera.flip")).is_none(), "closed");

            // Only what the camera can do: one front camera, without a light.
            h.state_mut().as_mut().unwrap().0.voice.camera.fake = Some(Controls::default());
            h.run_steps(2);
            h.get_by_label(t!("camera.camera")).click();
            h.run_steps(3);
            assert!(h.query_by_label(t!("camera.flip")).is_none());
            assert!(h.query_by_label(t!("camera.torch_on")).is_none());
            h.get_by_label(t!("call.camera_off")).click();
            h.run_steps(3);
            assert!(!home(&h).voice.camera.on);
            assert!(h.query_by_label(t!("call.camera_off")).is_none());

            // Off, a tap turns it straight back on.
            h.get_by_label(t!("call.camera_on")).click();
            h.run_steps(3);
            assert!(home(&h).voice.camera.on);
            assert!(h.query_by_label(t!("call.camera_off")).is_none(), "no menu");
        }
    }
}

/// The connection popup from the voice panel's signal bars: the channel's audio
/// quality by the connection state, and everything inside the window on a phone
/// (in the sidebar drawer) and on a desktop.
#[test]
fn connection_popup_shows_audio_quality_and_fits() {
    use crate::server::TrackStats;
    use rusttalk_core::client::voice::{CallTraffic, Traffic, VideoFormat};
    let data = tempfile::tempdir().unwrap();
    for (size, dark) in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)]
        .into_iter()
        .flat_map(|size| [(size, true), (size, false)])
    {
        let mut h = in_call(dark, size, data.path());
        let home = &mut h.state_mut().as_mut().unwrap().0;
        home.layout.list = true;
        home.servers[0].dir.categories[0].channels[0].audio_kbps = 96;
        let pair = |up: f64, down: f64, sent: u64, got: u64| {
            [
                Traffic {
                    bytes: sent,
                    bps: up * 1000.0,
                },
                Traffic {
                    bytes: got,
                    bps: down * 1000.0,
                },
            ]
        };
        let none = pair(0.0, 0.0, 0, 0);
        let track = |person, mine, kbps, video: Option<(u32, u32, f64)>| TrackStats {
            person,
            sid: format!("{mine}"),
            mine,
            screen: false,
            app: None,
            kbps,
            video: video.map(|(width, height, fps)| VideoFormat { width, height, fps }),
            detail: None,
            jitter_ms: None,
            lost: None,
            received: None,
        };
        let traffic = CallTraffic::from_sources([
            pair(95.0, 1.0, 1_090_000, 800_000),
            none,
            pair(0.0, 2_800.0, 0, 34_880_000),
            none,
            none,
        ]);
        for at in 0..120u32 {
            let spike = if at % 40 == 0 { 30.0 } else { 0.0 };
            let stats = Stats {
                ping_ms: Some(14.0 + f64::from(at % 7) + spike),
                lost_in: Some(i64::from(at / 30)),
                received_in: Some(u64::from(at) * 50),
                jitter_ms: Some(2.0),
                lost_out: Some(0),
                traffic,
                tracks: vec![
                    track(ME, true, 95.0, None),
                    track(ANN, false, 2_800.0, Some((1920, 1080, 30.0))),
                ],
            };
            home.voice.link.push(&stats);
            home.voice.call.as_mut().unwrap().stats = stats;
        }
        h.run_steps(3);
        h.get_by_label(t!("call.connection_info")).click();
        h.run_steps(4);
        let quality = format!("🎵  {} · 96 kbps", t!("home.audio_quality_label"));
        let settings = format!("⚙  {}", t!("call.voice_settings"));
        for label in [
            t!("call.connection"),
            &quality,
            t!("call.track_screen"),
            &settings,
        ] {
            let rect = h.get_by_label(label).rect();
            assert!(
                rect.min.x >= 0.0 && rect.max.x <= size.x && rect.max.y <= size.y,
                "{label} inside {size:?}: {rect:?}"
            );
        }
        // Kinds of traffic that never ran are left out of the table.
        assert!(h.query_by_label(t!("call.track_camera")).is_none());
        capture(&mut h, size, dark, "connection");
        h.get_by_label(t!("common.details")).click();
        h.run_steps(4);
        h.get_by_label(t!("call.jitter")).scroll_to_me();
        h.run_steps(4);
        let rect = h.get_by_label("1920×1080 · 30 fps · 2800 kbps").rect();
        assert!(rect.max.x <= size.x, "{rect:?}");
        capture(&mut h, size, dark, "connection-details");
    }
}

/// Android's call notification (#619): its buttons act without a frame (no window in
/// the background), and the next frame catches up without leaving twice.
#[test]
fn call_notification_buttons_work_without_a_frame() {
    use crate::call::notice::{self, Button, Shown};
    let (mut base, mut sent) = files_harness(true);
    let mut sent = move || std::iter::from_fn(|| sent.try_recv().ok()).collect::<Vec<_>>();
    let mut home = base.state_mut().take().unwrap();
    home.voice.call = Some(Call {
        server: home.servers[0].address.clone(),
        channel: PROJECTS,
        state: CallState::Connected,
        stats: Stats::default(),
        can_talk: true,
        can_share_video: true,
    });
    let shown = Shown {
        name: "projects".into(),
        server: "Friends".into(),
        muted: false,
        can_mute: true,
    };
    assert_eq!(notice::frame(&mut home), Some(shown.clone()));
    assert_eq!(notice::frame(&mut home), None, "unchanged: nothing to send");
    sent();

    assert!(notice::press(Button::Mute as i32));
    assert!(home.voice.audio.muted());
    assert!(matches!(sent()[..], [Command::SyncVoiceStatus]));
    let muted = notice::frame(&mut home).expect("the button changed");
    assert!(muted.muted);
    // Deafened counts as muted, and unmuting undeafens, like the app's mic button.
    home.voice.audio.set_muted(false);
    home.voice.audio.set_deafened(true);
    assert!(!notice::press(Button::Mute as i32));
    assert!(!home.voice.audio.deafened());
    assert!(matches!(sent()[..], [Command::SyncVoiceStatus]));
    assert_eq!(notice::frame(&mut home), Some(shown));
    assert!(!notice::press(7), "unknown buttons do nothing");
    assert!(sent().is_empty());

    // A moderator's mute: no Mute button, and a stale one does nothing.
    home.servers[0].dir.members.edit()[0].voice_muted = true;
    assert!(notice::frame(&mut home).is_some_and(|shown| !shown.can_mute));
    assert!(!notice::press(Button::Mute as i32));
    assert!(!home.voice.audio.muted());
    assert!(sent().is_empty());

    // Tapping it shows the call.
    home.view = View::Pokes;
    notice::open();
    assert_eq!(notice::frame(&mut home), None);
    assert!(matches!(home.view, View::Server(0)));
    assert_eq!(home.servers[0].open, Some(PROJECTS));
    sent();

    notice::press(Button::HangUp as i32);
    assert!(matches!(sent()[..], [Command::LeaveVoice]));
    assert!(home.voice.call.is_some(), "no frame ran yet");
    assert_eq!(notice::frame(&mut home), None);
    assert!(home.voice.call.is_none());
    assert!(sent().is_empty(), "left once");
    assert!(!notice::press(Button::HangUp as i32), "out of a call");
    assert!(sent().is_empty());
}

/// The call view's full-screen button covers the window with the whole call, also
/// with nobody sharing; the button again or Escape leaves, and the call goes on.
#[test]
fn the_whole_call_goes_full_screen() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let mut h = in_call(dark, size, data.path());
            assert!(
                h.query_by_label(t!("call.voice_connected")).is_some() || home(&h).layout.phone
            );
            h.get_by_label(t!("call.full_screen")).click();
            h.run_steps(3);
            assert!(home(&h).voice.stage.whole);
            // Only the call: no sidebar, no chat, and the way back.
            assert!(h.query_by_label(t!("call.voice_connected")).is_none());
            assert!(h.query_by_label(t!("layout.hide_chat")).is_none());
            assert!(h.query_by_label(t!("call.exit_full_screen")).is_some());
            // The call's buttons float over the bottom, and hide when idle.
            let leave = h.get_by_label(t!("call.leave")).rect();
            assert!(leave.bottom() > size.y - 80.0, "{size:?}: {leave:?}");
            capture(&mut h, size, dark, "whole");
            for _ in 0..2 {
                h.input_mut().time = Some(1000.0);
                h.step();
            }
            assert!(h.query_by_label(t!("call.leave")).is_none(), "{size:?}");
            capture(&mut h, size, dark, "whole-idle");
            h.hover_at(egui::pos2(size.x / 2.0, size.y / 2.0));
            for _ in 0..2 {
                h.input_mut().time = Some(1000.5);
                h.step();
            }
            assert!(h.query_by_label(t!("call.leave")).is_some(), "{size:?}");

            h.key_press(egui::Key::Escape);
            h.run_steps(3);
            assert!(!home(&h).voice.stage.whole);
            assert!(home(&h).voice.call.is_some());

            h.get_by_label(t!("call.full_screen")).click();
            h.run_steps(3);
            h.get_by_label(t!("call.exit_full_screen")).click();
            h.run_steps(3);
            assert!(!home(&h).voice.stage.whole);
        }
    }
}
