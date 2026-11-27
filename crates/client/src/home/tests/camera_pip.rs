//! Someone sharing camera and screen at once: the camera stays visible as a small
//! picture on the screen, which snaps to a corner, pops out and minimizes (#600).

use super::*;
use crate::call::pip::{Corner, Pip};
use crate::server::{Call, CallState, Peer, Screen as Shared, Stats};
use egui_kittest::{Harness, kittest::Queryable};

type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);

fn gradient(w: usize, h: usize, warm: bool) -> egui::ColorImage {
    let pixels = (0..w * h)
        .map(|at| {
            let (x, y) = (at % w, at / w);
            if warm {
                egui::Color32::from_rgb(180 + (x * 60 / w) as u8, 120, 80 + (y * 60 / h) as u8)
            } else {
                egui::Color32::from_rgb(40 + (x * 120 / w) as u8, 60, 90 + (y * 120 / h) as u8)
            }
        })
        .collect();
    egui::ColorImage::new([w, h], pixels)
}

/// In a call with Ann, who shares her camera and her screen; we watch the screen.
fn sharing(dark: bool, size: Vec2, data: &std::path::Path) -> Harness<'static, Option<Screen>> {
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
    let peer = |person, camera: Option<&str>, screens| Peer {
        person,
        connecting: false,
        unknown: false,
        status: Default::default(),
        apps: vec![],
        camera: camera.map(Into::into),
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
    home.servers[0].voice.insert(
        PROJECTS,
        vec![
            peer(ME, None, vec![]),
            peer(ANN, Some("ann-camera"), vec![screen]),
        ],
    );
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
    pictures(&mut h);
    h
}

/// New frames from Ann's screen and camera.
fn pictures(h: &mut Harness<'static, Option<Screen>>) {
    let video = &home(h).voice.video;
    video.put("remote", gradient(320, 180, false));
    video.put("ann-camera", gradient(160, 120, true));
    h.run_steps(20);
}

fn home<'a>(h: &'a Harness<'_, Option<Screen>>) -> &'a Home {
    &h.state().as_ref().unwrap().0
}

fn home_mut<'a>(h: &'a mut Harness<'_, Option<Screen>>) -> &'a mut Home {
    &mut h.state_mut().as_mut().unwrap().0
}

fn pip(h: &Harness<'_, Option<Screen>>) -> Pip {
    home(h).voice.pips.get(&ANN).copied().unwrap_or_default()
}

fn camera() -> String {
    t!("call.someones_camera", name = "Ann")
}

fn capture(h: &mut Harness<'_, Option<Screen>>, size: Vec2, dark: bool, state: &str) {
    if let Ok(dir) = std::env::var("RUSTTALK_PIP_SCREENSHOTS") {
        std::fs::create_dir_all(&dir).unwrap();
        let theme = if dark { "dark" } else { "light" };
        let name = format!("{dir}/{}x{}-{theme}-{state}.png", size.x, size.y);
        h.render().unwrap().save(name).unwrap();
    }
}

fn drag(h: &mut Harness<'static, Option<Screen>>, from: egui::Pos2, to: egui::Pos2) {
    h.hover_at(from);
    h.step();
    h.drag_at(from);
    h.step();
    for step in 1..=4 {
        h.hover_at(from + (to - from) * step as f32 / 4.0);
        h.step();
    }
    h.drop_at(to);
    h.run_steps(20);
}

#[test]
fn the_camera_shows_on_the_shared_screen_and_snaps_to_corners() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)] {
            let mut h = sharing(dark, size, data.path());
            // Both: the screen is watched and the camera is drawn on it, received.
            assert!(home(&h).voice.watching.is_some());
            capture(&mut h, size, dark, "stage");
            let start = h.get_by_label(&camera()).rect();
            assert!(home(&h).voice.cameras.contains_key("ann-camera"));
            assert_eq!(pip(&h).corner, Corner::BottomRight);

            // Dragged towards the top left, it lands in that corner.
            let to = start.center() - Vec2::new(size.x * 0.6, size.y * 0.3);
            drag(&mut h, start.center(), to);
            assert_eq!(pip(&h).corner, Corner::TopLeft);
            let moved = h.get_by_label(&camera()).rect();
            assert!(moved.left() < start.left() && moved.top() < start.top());
            // Clear of the tile's own buttons, which show while the pointer is over it.
            h.hover_at(moved.center());
            h.run_steps(2);
            let stop = h.get_by_label(t!("call.stop_watching")).rect();
            assert!(!stop.intersects(moved));
            capture(&mut h, size, dark, "hover");

            // Minimized: a pill in its corner, and the camera is no longer received.
            h.get_by_role_and_label(egui::Role::Button, t!("call.minimize_camera"))
                .click();
            h.run_steps(4);
            assert!(pip(&h).minimized);
            assert!(h.query_by_label(&camera()).is_none());
            assert!(!home(&h).voice.cameras.contains_key("ann-camera"));
            let pill = t!("call.show_camera", name = "Ann");
            // In the corner it was dragged to.
            let at = h.get_by_label(&pill).rect();
            assert!((at.min - moved.min).length() < 1.0, "{at:?} {moved:?}");
            capture(&mut h, size, dark, "minimized");
            h.get_by_label(&pill).click();
            h.run_steps(4);
            assert!(!pip(&h).minimized);
            pictures(&mut h);
            assert!(h.query_by_label(&camera()).is_some());

            // Popped out: its own tile beside the screen, with a way back.
            let at = h.get_by_label(&camera()).rect().center();
            h.hover_at(at);
            h.run_steps(2);
            h.get_by_role_and_label(egui::Role::Button, t!("call.pop_out_camera"))
                .click();
            h.run_steps(4);
            pictures(&mut h);
            assert!(pip(&h).popped);
            assert!(h.query_by_label(&camera()).is_none());
            // Her camera tile, the largest of her buttons (the side list has one too).
            let tile = h
                .get_all_by_role_and_label(egui::Role::Button, "Ann")
                .map(|node| node.rect())
                .max_by(|a, b| a.area().total_cmp(&b.area()))
                .unwrap();
            assert!(tile.height() > 40.0);
            h.hover_at(tile.center());
            h.run_steps(2);
            capture(&mut h, size, dark, "popped");
            h.get_by_role_and_label(egui::Role::Button, t!("call.camera_on_screen"))
                .click();
            h.run_steps(4);
            pictures(&mut h);
            assert_eq!(
                pip(&h),
                Pip {
                    corner: Corner::TopLeft,
                    popped: false,
                    minimized: false
                }
            );
            assert!(h.query_by_label(&camera()).is_some());
        }
    }
}

#[test]
fn the_camera_keeps_its_corner_in_full_screen_clear_of_the_controls() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)] {
            let mut h = sharing(dark, size, data.path());
            home_mut(&mut h).voice.pips.insert(
                ANN,
                Pip {
                    corner: Corner::TopRight,
                    ..Pip::default()
                },
            );
            home_mut(&mut h)
                .voice
                .stage
                .set_presenting_and_full_screen("remote");
            pictures(&mut h);
            h.hover_at(egui::pos2(size.x / 2.0, size.y / 2.0));
            h.run_steps(2);
            let pip = h.get_by_label(&camera()).rect();
            assert!(pip.center().x > size.x / 2.0 && pip.center().y < size.y / 2.0);
            let controls = [
                t!("call.exit_full_screen"),
                t!("call.stream_options"),
                t!("call.mute"),
                t!("call.leave"),
            ];
            for label in controls {
                assert!(!h.get_by_label(label).rect().intersects(pip), "{label}");
            }
            capture(&mut h, size, dark, "full-screen");
        }
    }
}

/// We share our camera and screen too, previewing the screen. The self view is the
/// fake camera's picture (`Camera::fake`): the real one needs a device.
fn share_own(h: &mut Harness<'static, Option<Screen>>) {
    let ctx = h.ctx.clone();
    let home = home_mut(h);
    let me = &mut home.servers[0].voice.get_mut(&PROJECTS).unwrap()[0];
    me.camera = Some("my camera".into());
    me.screens = vec![Shared {
        sid: "local".into(),
        name: "Display".into(),
        viewers: 0,
        local: true,
        control: None,
    }];
    home.voice.sharing = call::Sharing::On;
    let camera = &mut home.voice.camera;
    let picture = gradient(160, 120, true);
    camera.fake_picture(ctx.load_texture("fake camera", picture, Default::default()));
    camera.fake = Some(Default::default());
    camera.on = true;
    h.run_steps(6);
}

fn my_camera() -> String {
    t!("call.someones_camera", name = "Me")
}

fn my_pip(h: &Harness<'_, Option<Screen>>) -> Pip {
    home(h).voice.pips.get(&ME).copied().unwrap_or_default()
}

/// Pop our camera out of its picture on our screen, as a person does: hover, click.
fn pop_out_mine(h: &mut Harness<'static, Option<Screen>>) {
    let at = h.get_by_label(&my_camera()).rect().center();
    h.hover_at(at);
    h.run_steps(2);
    h.get_by_role_and_label(egui::Role::Button, t!("call.pop_out_camera"))
        .click();
    h.run_steps(6);
}

/// Our popped-out camera is a stage tile drawn with the self view (the warm test
/// picture, not an avatar), and "Put back on the screen" returns it.
fn my_tile_shows_and_goes_back(h: &mut Harness<'static, Option<Screen>>) {
    assert!(my_pip(h).popped);
    assert!(h.query_by_label(&my_camera()).is_none());
    // The largest of my buttons: the side list has one too.
    let tile = h
        .get_all_by_role_and_label(egui::Role::Button, "Me")
        .map(|node| node.rect())
        .max_by(|a, b| a.area().total_cmp(&b.area()))
        .expect("a tile for my popped-out camera");
    assert!(tile.height() > 40.0, "{tile:?}");
    // Reading pixels needs a GPU; CI runners have none, so there the check is skipped.
    let render = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.render()));
    if let Ok(Ok(image)) = render {
        let ppp = h.ctx.pixels_per_point();
        let spot = tile.center();
        let pixel = image.get_pixel((spot.x * ppp) as u32, (spot.y * ppp) as u32);
        assert!(pixel[0] > 150 && pixel[0] > pixel[2], "{pixel:?}");
    }
    h.hover_at(tile.center());
    h.run_steps(2);
    h.get_by_role_and_label(egui::Role::Button, t!("call.camera_on_screen"))
        .click();
    h.run_steps(6);
    assert!(!my_pip(h).popped);
    assert!(h.query_by_label(&my_camera()).is_some());
}

#[test]
fn your_own_camera_pops_out_of_your_screen_preview_and_back() {
    let data = tempfile::tempdir().unwrap();
    let mut h = sharing(true, Vec2::new(1100.0, 700.0), data.path());
    share_own(&mut h);
    pop_out_mine(&mut h);
    my_tile_shows_and_goes_back(&mut h);
}

/// Full screen has no room beside the screen: popping out leaves it, so the camera
/// shows as its own tile instead of vanishing (#604).
#[test]
fn popping_out_your_camera_in_full_screen_shows_its_tile() {
    let data = tempfile::tempdir().unwrap();
    for dark in [true, false] {
        for size in [Vec2::new(1100.0, 700.0), Vec2::new(390.0, 844.0)] {
            let mut h = sharing(dark, size, data.path());
            share_own(&mut h);
            home_mut(&mut h).voice.stage.set_full_screen_preview();
            h.run_steps(6);
            capture(&mut h, size, dark, "own-full-screen");
            pop_out_mine(&mut h);
            assert!(home(&h).voice.stage.full.is_none());
            capture(&mut h, size, dark, "own-popped");
            my_tile_shows_and_goes_back(&mut h);
        }
    }
}

/// Our camera on, without sharing a screen; the self view is the fake camera's picture.
fn camera_on(h: &mut Harness<'static, Option<Screen>>) {
    let ctx = h.ctx.clone();
    let home = home_mut(h);
    home.servers[0].voice.get_mut(&PROJECTS).unwrap()[0].camera = Some("my camera".into());
    let camera = &mut home.voice.camera;
    let picture = gradient(120, 160, true);
    camera.fake_picture(ctx.load_texture("fake camera", picture, Default::default()));
    camera.fake = Some(Default::default());
    camera.on = true;
    h.run_steps(6);
}

fn self_view(h: &Harness<'_, Option<Screen>>) -> bool {
    h.query_by_label(t!("call.your_camera")).is_some()
}

/// On a phone our camera stays in sight: a small self view floats while our own tile is
/// off screen, and it minimizes to a pill (#656).
#[test]
fn phones_keep_your_camera_in_sight_outside_your_tile() {
    let data = tempfile::tempdir().unwrap();
    let phone = Vec2::new(390.0, 844.0);
    for dark in [true, false] {
        let mut h = sharing(dark, phone, data.path());
        camera_on(&mut h);
        // The call view shows our tile, so no self view on top of it.
        assert!(!self_view(&h));

        // Watching Ann's screen in full screen hides our tile: the self view appears.
        home_mut(&mut h)
            .voice
            .stage
            .set_presenting_and_full_screen("remote");
        h.run_steps(20);
        capture(&mut h, phone, dark, "self-view-full-screen");
        assert!(self_view(&h));

        // So it does over the channel list, with the call going on.
        home_mut(&mut h).voice.stage.full = None;
        home_mut(&mut h).layout.list = true;
        h.run_steps(20);
        capture(&mut h, phone, dark, "self-view-list");
        assert!(self_view(&h));

        // Dragged up and left, it lands in the top left corner, over the list.
        let at = h.get_by_label(t!("call.your_camera")).rect().center();
        drag(&mut h, at, egui::pos2(40.0, 120.0));
        assert_eq!(my_pip(&h).corner, Corner::TopLeft);
        capture(&mut h, phone, dark, "self-view-dragged");

        // Minimized, it is a pill that brings it back.
        h.get_by_label(t!("call.your_camera")).hover();
        h.run_steps(4);
        h.get_by_label(t!("call.minimize_camera")).click();
        h.run_steps(20);
        assert!(my_pip(&h).minimized && !self_view(&h));
        h.get_by_label(t!("call.show_your_camera")).click();
        h.run_steps(20);
        assert!(!my_pip(&h).minimized && self_view(&h));

        // Camera off: nothing floats.
        home_mut(&mut h).voice.camera.on = false;
        h.run_steps(6);
        assert!(!self_view(&h));
    }
}

/// Desktops keep their layout: no floating self view.
#[test]
fn desktops_have_no_floating_self_view() {
    let data = tempfile::tempdir().unwrap();
    let mut h = sharing(true, Vec2::new(1100.0, 700.0), data.path());
    camera_on(&mut h);
    home_mut(&mut h)
        .voice
        .stage
        .set_presenting_and_full_screen("remote");
    h.run_steps(20);
    assert!(!self_view(&h));
}

/// A phone turned sideways for a full-screen stream still shows the self view (#656).
#[test]
fn a_sideways_phone_keeps_your_camera_in_full_screen() {
    let data = tempfile::tempdir().unwrap();
    let sideways = Vec2::new(844.0, 390.0);
    let mut h = sharing(true, sideways, data.path());
    // A touch screen: one tap where nothing is.
    for phase in [egui::TouchPhase::Start, egui::TouchPhase::End] {
        h.input_mut().events.push(egui::Event::Touch {
            device_id: egui::TouchDeviceId(1),
            id: egui::TouchId(1),
            phase,
            pos: egui::pos2(4.0, 4.0),
            force: None,
        });
        h.step();
    }
    camera_on(&mut h);
    home_mut(&mut h)
        .voice
        .stage
        .set_presenting_and_full_screen("remote");
    h.run_steps(20);
    capture(&mut h, sideways, true, "self-view-sideways");
    assert!(self_view(&h));
    // Clear of the full-screen buttons at the top and bottom.
    let rect = h.get_by_label(t!("call.your_camera")).rect();
    assert!(rect.top() >= ui::TOUCH_TARGET && rect.bottom() <= sideways.y - ui::TOUCH_TARGET);
}

/// Turning the phone sideways while in full screen keeps the self view (#656).
#[test]
fn turning_the_phone_keeps_your_camera_in_full_screen() {
    let data = tempfile::tempdir().unwrap();
    let mut h = sharing(true, Vec2::new(390.0, 844.0), data.path());
    for phase in [egui::TouchPhase::Start, egui::TouchPhase::End] {
        h.input_mut().events.push(egui::Event::Touch {
            device_id: egui::TouchDeviceId(1),
            id: egui::TouchId(1),
            phase,
            pos: egui::pos2(4.0, 4.0),
            force: None,
        });
        h.step();
    }
    camera_on(&mut h);
    home_mut(&mut h)
        .voice
        .stage
        .set_presenting_and_full_screen("remote");
    h.run_steps(20);
    assert!(self_view(&h), "upright");
    h.set_size(Vec2::new(844.0, 390.0));
    h.run_steps(20);
    capture(&mut h, Vec2::new(844.0, 390.0), true, "self-view-turned");
    assert!(self_view(&h), "sideways");
}

/// A phone held sideways: the voice panel keeps to one row, so the channel list and
/// the rail keep room (#693).
#[test]
fn a_low_window_keeps_room_for_the_channels() {
    let data = tempfile::tempdir().unwrap();
    for (size, short) in [
        (Vec2::new(914.0, 380.0), true),
        (Vec2::new(1100.0, 700.0), false),
    ] {
        let mut h = sharing(true, size, data.path());
        h.run_steps(10);
        capture(&mut h, size, true, "sidebar");
        // The status line with its signal and place shows only when there is room.
        let status = h
            .query_by_label_contains(t!("call.voice_connected"))
            .is_some();
        assert_eq!(status, !short, "{size:?}");
        assert!(h.query_by_label(t!("call.leave")).is_some(), "{size:?}");
        // The sidebar's voice panel starts below the middle, leaving the list room.
        let leave = h.get_by_label(t!("call.leave")).rect();
        assert!(!short || leave.top() > size.y * 0.6, "{size:?} {leave:?}");
    }
}

/// A phone held sideways with only the chat shown: the self view stays beside the
/// sidebar, also when dragged into the bottom left, so it never covers the voice panel
/// (#692).
#[test]
fn sideways_the_self_view_stays_off_the_voice_panel() {
    let data = tempfile::tempdir().unwrap();
    let size = Vec2::new(914.0, 380.0);
    let mut h = sharing(true, size, data.path());
    for phase in [egui::TouchPhase::Start, egui::TouchPhase::End] {
        h.input_mut().events.push(egui::Event::Touch {
            device_id: egui::TouchDeviceId(1),
            id: egui::TouchId(1),
            phase,
            pos: egui::pos2(4.0, 4.0),
            force: None,
        });
        h.step();
    }
    camera_on(&mut h);
    let mut split = home(&h).layout.call_split();
    split.stage_hidden = true;
    home_mut(&mut h).layout.set_call_split(split);
    h.run_steps(20);
    capture(&mut h, size, true, "self-view-sideways-chat");
    assert!(self_view(&h));
    let at = h.get_by_label(t!("call.your_camera")).rect().center();
    drag(&mut h, at, egui::pos2(10.0, size.y - 10.0));
    h.run_steps(20);
    capture(&mut h, size, true, "self-view-bottom-left");
    assert_eq!(my_pip(&h).corner, Corner::BottomLeft);
    let picture = h.get_by_label(t!("call.your_camera")).rect();
    let leave = h.get_by_label(t!("call.leave")).rect();
    assert!(
        picture.left() >= leave.right(),
        "{picture:?} over {leave:?}"
    );
}
