//! Regression tests and offscreen visual captures for rounded-square avatars,
//! bottom-left local speaking state, and sidebar server hover feedback (#361).

use super::*;
use crate::server::{Call, CallState, Category, Channel, Member, Peer, Stats};
use eframe::egui::{Event, Modifiers, PointerButton, Pos2, TouchDeviceId, TouchId, TouchPhase};
use egui_kittest::{Harness, kittest::Queryable};
use image::{ImageFormat, Rgba, RgbaImage};
use std::io::Cursor;

const LOUNGE: Id = [2; 32];
const GENERAL: Id = [1; 32];
const DESIGN_GENERAL: Id = [4; 32];
const RUST_GENERAL: Id = [5; 32];

type Screen = (
    Home,
    crate::update::Updater,
    crate::keystore::Keystore,
    tempfile::TempDir,
);

fn sample_picture(key: u8, primary: [u8; 4], accent: [u8; 4]) -> Picture {
    let mut img = RgbaImage::new(64, 64);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let diag = (x + y) / 16 % 2 == 0;
        let border = x < 6 || y < 6 || x >= 58 || y >= 58;
        *px = Rgba(if border || diag { primary } else { accent });
    }
    let mut bytes = Cursor::new(Vec::new());
    img.write_to(&mut bytes, ImageFormat::Png).unwrap();
    let image = ColorImage::from_rgba_unmultiplied([64, 64], img.as_raw());
    Picture {
        key: [key; 32],
        frames: Arc::new(vec![(image, 0)]),
        bytes: Arc::new(bytes.into_inner()),
    }
}

fn channel(id: Id, name: &str, voice: bool) -> Channel {
    Channel {
        id,
        name: name.into(),
        topic: String::new(),
        voice,
        page: false,
        forum: false,
        audio_kbps: 64,
        flags: Vec::new(),
    }
}

fn demo_window(size: Vec2, dark: bool) -> Harness<'static, Option<Screen>> {
    let mut harness = Harness::builder()
        .with_size(size)
        .with_step_dt(0.05)
        .build_ui_state(
            |ui, screen: &mut Option<Screen>| {
                if let Some((home, updates, keystore, _dir)) = screen {
                    super::super::show(ui, home, updates, keystore);
                }
            },
            None,
        );
    ui::install(&harness.ctx);
    let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Desktop").unwrap();
    let me_id = *identity.certificate.person_id();
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().to_path_buf();
    let mut home = Home::new(identity, &harness.ctx, data.clone()).unwrap();
    let mut settings = home.voice.audio.settings();
    settings.theme_mode = if dark { "dark" } else { "light" }.into();
    settings.call_sounds.volume = 0.0;
    home.voice.audio.set_settings(settings);
    let mut prefs = home.notify.prefs().clone();
    prefs.desktop = false;
    home.notify.set_prefs(prefs);

    let me_pic = sample_picture(11, [0x2b, 0x63, 0xc6, 255], [0x7e, 0xaa, 0xf5, 255]);
    let friends_icon = sample_picture(21, [0xb7, 0x41, 0x0e, 255], [0xf0, 0x8a, 0x4b, 255]);
    let design_icon = sample_picture(22, [0x1d, 0x7a, 0x45, 255], [0x5f, 0xcf, 0x8e, 255]);

    let make_server = |idx: u8, url: &str, name: &str, icon: Option<Picture>, chan: Id| {
        let (commands, _sent) = tokio::sync::mpsc::unbounded_channel();
        let known = KnownServer {
            address: url.parse().unwrap(),
            server_id: [idx; 32],
            tls_fingerprint: None,
        };
        let mut server = super::super::new_server(known, name, commands);
        server.status = Status::Online;
        server.dir.me = me_id;
        server.dir.icon = icon;
        server.dir.members = vec![
            Member {
                person: me_id,
                name: Some("Alex".into()),
                devices: Vec::new(),
                roles: Vec::new(),
                avatar: Some(me_pic.clone()),
                presence: Presence::Online,
                hide_offline: false,
                bio: None,
                voice_muted: false,
            },
            Member {
                person: ANN,
                name: Some("Ann".into()),
                devices: Vec::new(),
                roles: Vec::new(),
                avatar: None,
                presence: Presence::Away,
                hide_offline: false,
                bio: None,
                voice_muted: false,
            },
        ];
        server.dir.categories = vec![Category {
            id: None,
            name: String::new(),
            channels: vec![channel(chan, "general", false)],
        }];
        server.open = Some(chan);
        server
    };

    let mut friends = make_server(
        2,
        "https://friends.example.org/",
        "Friends",
        Some(friends_icon),
        GENERAL,
    );
    friends.dir.categories[0]
        .channels
        .push(channel(LOUNGE, "Lounge", true));
    friends.voice.insert(
        LOUNGE,
        vec![
            Peer {
                person: me_id,
                connecting: false,
                unknown: false,
                status: Default::default(),
                apps: Vec::new(),
                screens: Vec::new(),
                camera: None,
                terminal: None,
            },
            Peer {
                person: ANN,
                connecting: false,
                unknown: false,
                status: Default::default(),
                apps: Vec::new(),
                screens: Vec::new(),
                camera: None,
                terminal: None,
            },
        ],
    );
    let mut chat = Chat::default();
    chat.opened = true;
    chat.messages = vec![
        Msg {
            id: [10; 32],
            person: me_id,
            at: 1_700_000_000,
            body: Body::Text("Testing rounded-square avatars and local speaker highlight.".into()),
            backfilled: false,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            files: Vec::new(),
            live: None,
            sticker: None,
            replicas: Vec::new(),
            copy_files: Vec::new(),
        },
        Msg {
            id: [11; 32],
            person: ANN,
            at: 1_700_000_060,
            body: Body::Text("Initials avatars match the same rounded square!".into()),
            backfilled: false,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            files: Vec::new(),
            live: None,
            sticker: None,
            replicas: Vec::new(),
            copy_files: Vec::new(),
        },
    ];
    friends.chats.insert(GENERAL, chat);

    let design = make_server(
        3,
        "https://design.example.org/",
        "Design",
        Some(design_icon),
        DESIGN_GENERAL,
    );
    let rust = make_server(
        4,
        "https://rust.example.org/",
        "Rust Crew",
        None,
        RUST_GENERAL,
    );

    let future = crate::notify::unix_now() + 100;
    home.notify
        .arrived(&design.address, DESIGN_GENERAL, future, true);
    home.notify
        .arrived(&design.address, DESIGN_GENERAL, future + 1, true);
    home.notify
        .arrived(&design.address, DESIGN_GENERAL, future + 2, true);
    home.notify
        .arrived(&rust.address, RUST_GENERAL, future, false);

    home.voice.call = Some(Call {
        server: friends.address.clone(),
        channel: LOUNGE,
        state: CallState::Connected,
        stats: Stats::default(),
        can_talk: true,
        can_share_video: true,
    });
    home.voice.audio.set_talked_for_test(true);

    home.servers = vec![friends, design, rust];
    home.view = View::Server(0);
    if size.x < ui::PHONE_WIDTH {
        home.layout.list = true;
    }

    *harness.state_mut() = Some((
        home,
        crate::update::Updater::new(&data),
        crate::keystore::without_keychain(&data),
        dir,
    ));
    harness.run_steps(4);
    harness
}

fn rect_shapes_in(
    h: &Harness<'static, Option<Screen>>,
    area: Rect,
) -> Vec<eframe::egui::epaint::RectShape> {
    h.output()
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            Shape::Rect(r) if area.intersects(r.rect) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

fn has_rail_hover_outline(shapes: &[eframe::egui::epaint::RectShape], button: Rect) -> bool {
    let expected = button.expand(1.5);
    shapes.iter().any(|r| {
        (r.rect.left() - expected.left()).abs() < 0.5
            && (r.rect.top() - expected.top()).abs() < 0.5
            && (r.rect.width() - expected.width()).abs() < 0.5
            && (r.rect.height() - expected.height()).abs() < 0.5
            && r.stroke.width > 0.0
            && r.stroke.color.a() > 0
    })
}

fn has_rail_hover_sheen(shapes: &[eframe::egui::epaint::RectShape], button: Rect) -> bool {
    shapes.iter().any(|r| {
        (r.rect.left() - button.left()).abs() < 0.5
            && (r.rect.top() - button.top()).abs() < 0.5
            && (r.rect.width() - button.width()).abs() < 0.5
            && (r.rect.height() - button.height()).abs() < 0.5
            && r.fill.a() > 0
            && r.fill.a() < 255
    })
}

fn home_mut<'a>(h: &'a mut Harness<'static, Option<Screen>>) -> &'a mut Home {
    &mut h.state_mut().as_mut().unwrap().0
}

#[test]
fn avatar_radius_scales_consistently_as_rounded_square() {
    for size in [
        16.0, 22.0, 24.0, 30.0, 32.0, 38.0, 40.0, 48.0, 56.0, 64.0, 96.0, 300.0,
    ] {
        let r = ui::avatar_radius(size);
        assert!(
            (r - size * 0.25).abs() < 1e-4,
            "size {size} should use 25% rounded-square radius, got {r}"
        );
        assert!(r < size * 0.5, "avatar radius must never be a full circle");
    }
    assert_eq!(ui::avatar_radius(0.0), 0.0);
    assert_eq!(ui::avatar_radius(-10.0), 0.0);
    assert!(rail_radius(1.0) < rail_radius(0.0));
    assert!(rail_radius(1.0) >= 8.0);
}

#[test]
fn canonical_local_speaking_state_covers_voice_modes_and_call_transitions() {
    let mut h = demo_window(Vec2::new(1100.0, 700.0), true);
    let home = home_mut(&mut h);

    // Active connected call with speech -> speaking.
    home.voice.audio.set_talked_for_test(true);
    assert!(home.voice.local_speaking(&home.servers));

    // Viewing another server still lights up the local account avatar while in the call.
    home.view = View::Server(1);
    assert!(home.voice.local_speaking(&home.servers));
    home.view = View::Server(0);

    // Connecting or reconnecting suppresses speaking.
    home.voice.call.as_mut().unwrap().state = CallState::Connecting;
    assert!(!home.voice.local_speaking(&home.servers));
    home.voice.call.as_mut().unwrap().state = CallState::Reconnecting;
    assert!(!home.voice.local_speaking(&home.servers));
    home.voice.call.as_mut().unwrap().state = CallState::Connected;
    assert!(home.voice.local_speaking(&home.servers));

    // Listen-only channel suppresses speaking.
    home.voice.call.as_mut().unwrap().can_talk = false;
    assert!(!home.voice.local_speaking(&home.servers));
    home.voice.call.as_mut().unwrap().can_talk = true;
    assert!(home.voice.local_speaking(&home.servers));

    // Moderator voice mute suppresses speaking.
    home.servers[0].dir.members[0].voice_muted = true;
    assert!(!home.voice.local_speaking(&home.servers));
    home.servers[0].dir.members[0].voice_muted = false;
    assert!(home.voice.local_speaking(&home.servers));

    // Local mute and deafen suppress speaking immediately.
    home.voice.audio.set_muted(true);
    assert!(!home.voice.local_speaking(&home.servers));
    home.voice.audio.set_muted(false);
    home.voice.audio.set_talked_for_test(true);
    assert!(home.voice.local_speaking(&home.servers));

    home.voice.audio.set_deafened(true);
    assert!(!home.voice.local_speaking(&home.servers));
    home.voice.audio.set_deafened(false);
    home.voice.audio.set_talked_for_test(true);
    assert!(home.voice.local_speaking(&home.servers));

    // Phone withdrawal interlock pauses channel mic and suppresses speaking.
    home.voice.audio.request_phone_withdrawal();
    assert!(!home.voice.local_speaking(&home.servers));
    home.voice.audio.pause_phone_withdrawal(false);
    home.voice.audio.accept_phone_withdrawal_request();
    assert!(home.voice.local_speaking(&home.servers));

    // Push-to-talk requires holding key_down or hotkey_down.
    let mut settings = home.voice.audio.settings();
    settings.push_to_talk = true;
    home.voice.audio.set_settings(settings.clone());
    assert!(!home.voice.local_speaking(&home.servers));
    home.voice.audio.set_key_down(true);
    assert!(home.voice.local_speaking(&home.servers));
    home.voice.audio.set_key_down(false);
    assert!(!home.voice.local_speaking(&home.servers));
    home.voice.audio.set_hotkey_down(true);
    assert!(!home.voice.local_speaking(&home.servers));
    home.voice.audio.set_talked_for_test(true);
    assert!(home.voice.local_speaking(&home.servers));
    home.voice.audio.set_hotkey_down(false);
    assert!(!home.voice.local_speaking(&home.servers));
    settings.push_to_talk = false;
    home.voice.audio.set_settings(settings);

    // Losing / ending the call stops local speaking even if mic testing triggers talked.
    home.voice.end_call();
    home.voice.audio.set_talked_for_test(true);
    assert!(!home.voice.local_speaking(&home.servers));
}

#[test]
fn bottom_left_avatar_lights_up_on_speaking_transitions_in_both_themes() {
    for dark in [true, false] {
        let mut h = demo_window(Vec2::new(1100.0, 700.0), dark);
        let account_label = t!("status.button", name = "Alex", status = t!("status.online"));
        let account_rect = h
            .get_by_role_and_label(egui::Role::Button, &account_label)
            .rect();
        let avatar_band =
            Rect::from_min_size(account_rect.min, Vec2::new(50.0, account_rect.height()));

        let find_presence_dot = |h: &Harness<'static, Option<Screen>>| {
            h.output()
                .shapes
                .iter()
                .enumerate()
                .find_map(|(i, s)| match &s.shape {
                    Shape::Circle(c)
                        if avatar_band.contains(c.center) && (c.radius - 5.0).abs() < 0.5 =>
                    {
                        Some((i, c.fill))
                    }
                    _ => None,
                })
        };
        let find_speaking_ring = |h: &Harness<'static, Option<Screen>>, success: Color32| {
            h.output()
                .shapes
                .iter()
                .enumerate()
                .find_map(|(i, s)| match &s.shape {
                    Shape::Rect(r)
                        if avatar_band.expand(4.0).intersects(r.rect)
                            && (r.stroke.width - 2.0).abs() < 0.2
                            && r.stroke.color == success
                            && r.fill == Color32::TRANSPARENT =>
                    {
                        Some(i)
                    }
                    _ => None,
                })
        };

        home_mut(&mut h).voice.audio.set_talked_for_test(false);
        h.run_steps(2);
        let (_, success) = find_presence_dot(&h).expect("online presence dot must be painted");
        assert!(
            find_speaking_ring(&h, success).is_none(),
            "silent bottom-left avatar must not paint a speaking outline (dark={dark})"
        );

        home_mut(&mut h).voice.audio.set_talked_for_test(true);
        h.run_steps(2);
        let (dot_idx, success) =
            find_presence_dot(&h).expect("online presence dot must be painted");
        let ring_idx = find_speaking_ring(&h, success).unwrap_or_else(|| {
            panic!("bottom-left avatar must paint speaking outline (dark={dark})")
        });
        assert!(
            ring_idx < dot_idx,
            "presence dot must paint on top of speaking outline (dark={dark})"
        );

        // Muting extinguishes the speaking outline back to silent state.
        home_mut(&mut h).voice.audio.set_muted(true);
        h.run_steps(2);
        assert!(
            find_speaking_ring(&h, success).is_none(),
            "muting must clear the bottom-left speaking outline (dark={dark})"
        );

        // Hit target stays identical across speaking transitions.
        let after_rect = h
            .get_by_role_and_label(egui::Role::Button, &account_label)
            .rect();
        assert_eq!(account_rect, after_rect);
    }
}

#[test]
fn sidebar_server_hover_gives_subtle_feedback_without_layout_shift_and_clears_on_exit() {
    for dark in [true, false] {
        let mut h = demo_window(Vec2::new(1100.0, 700.0), dark);
        home_mut(&mut h).voice.audio.set_talked_for_test(false);
        h.run_steps(4);

        for name in ["Design", "Rust Crew"] {
            let before_rect = h.get_by_role_and_label(egui::Role::Button, name).rect();
            let probe = before_rect.expand(4.0);
            // Move pointer away to capture rest state.
            h.hover_at(Pos2::new(600.0, 350.0));
            h.run_steps(8);
            let rest = rect_shapes_in(&h, probe);
            assert!(
                !has_rail_hover_outline(&rest, before_rect),
                "unselected {name} must have no hover outline at rest (dark={dark})"
            );

            // Hover over server icon: visuals change, rect does NOT shift.
            h.hover_at(before_rect.center());
            h.run_steps(8);
            let hovered_rect = h.get_by_role_and_label(egui::Role::Button, name).rect();
            assert_eq!(
                before_rect, hovered_rect,
                "hovering {name} must not shift layout"
            );
            let hovered = rect_shapes_in(&h, probe);
            assert!(
                has_rail_hover_outline(&hovered, before_rect),
                "hovering {name} must paint subtle outer hover outline (dark={dark})"
            );
            assert_ne!(
                rest, hovered,
                "hovering {name} must give visible feedback (dark={dark})"
            );

            // Hover exit: smoothly returns to exact rest shapes.
            h.hover_at(Pos2::new(600.0, 350.0));
            h.run_steps(8);
            let exited = rect_shapes_in(&h, probe);
            assert_eq!(
                rest, exited,
                "hover exit on {name} must restore rest appearance (dark={dark})"
            );
        }

        // Hovering an unavailable server preserves the "!" badge and tooltip while giving feedback.
        home_mut(&mut h).servers[2].status = Status::Offline;
        h.run_steps(4);
        let offline_rect = h
            .get_by_role_and_label(egui::Role::Button, "Rust Crew")
            .rect();
        h.hover_at(offline_rect.center());
        h.run_steps(8);
        assert_eq!(
            h.get_by_role_and_label(egui::Role::Button, "Rust Crew")
                .rect(),
            offline_rect
        );
        assert!(h.output().shapes.iter().any(|shape| {
            matches!(
                &shape.shape,
                Shape::Text(text) if offline_rect.contains(text.pos) && text.galley.text() == "!"
            )
        }));
        h.get_by_label(t!("home.server_unavailable"));

        // Clicking still navigates to the hovered server cleanly.
        h.get_by_role_and_label(egui::Role::Button, "Design")
            .click();
        h.run_steps(4);
        assert!(home_mut(&mut h).view == View::Server(1));
    }
}

#[test]
fn touch_tap_on_server_icon_navigates_without_stuck_hover_and_fits_phone_tablet_desktop() {
    for dark in [true, false] {
        for size in [
            Vec2::new(390.0, 844.0),
            Vec2::new(820.0, 1180.0),
            Vec2::new(1100.0, 700.0),
        ] {
            let mut h = demo_window(size, dark);
            let design_rect = h.get_by_role_and_label(egui::Role::Button, "Design").rect();
            let pos = design_rect.center();
            assert!(
                pos.x >= 0.0 && pos.x <= size.x && pos.y >= 0.0 && pos.y <= size.y,
                "server icon must fit within viewport {size:?}"
            );
            // Simulate a real touch tap (Start -> End + PointerGone).
            let events = &mut h.input_mut().events;
            events.push(Event::Touch {
                device_id: TouchDeviceId(0),
                id: TouchId(0),
                phase: TouchPhase::Start,
                pos,
                force: None,
            });
            events.push(Event::PointerMoved(pos));
            events.push(Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            });
            h.step();
            let events = &mut h.input_mut().events;
            events.push(Event::Touch {
                device_id: TouchDeviceId(0),
                id: TouchId(0),
                phase: TouchPhase::End,
                pos,
                force: None,
            });
            events.push(Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            });
            events.push(Event::PointerGone);
            h.run_steps(8);
            assert!(home_mut(&mut h).view == View::Server(1));
            let after_shapes = rect_shapes_in(&h, design_rect.expand(4.0));
            assert!(
                !has_rail_hover_sheen(&after_shapes, design_rect),
                "touch tap must not leave hover sheen stuck on Design (dark={dark}, size={size:?})"
            );
        }
    }
}

#[test]
fn avatar_surfaces_capture_screenshots() {
    let Ok(dir) = std::env::var("RUSTTALK_AVATAR_SCREENSHOTS") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for (theme, dark) in [("dark", true), ("light", false)] {
        for (tag, size) in [
            ("desktop-1100x700", Vec2::new(1100.0, 700.0)),
            ("phone-390x844", Vec2::new(390.0, 844.0)),
        ] {
            let mut h = demo_window(size, dark);
            home_mut(&mut h).servers[2].status = Status::Offline;
            home_mut(&mut h).voice.audio.set_talked_for_test(true);
            h.run_steps(2);
            if size.x >= ui::DESKTOP_WIDTH {
                let design_pos = h
                    .get_by_role_and_label(egui::Role::Button, "Design")
                    .rect()
                    .center();
                h.hover_at(design_pos);
                h.run_steps(6);
                home_mut(&mut h).voice.audio.set_talked_for_test(true);
                h.step();
            }
            h.render()
                .unwrap()
                .save(format!("{dir}/{tag}-{theme}.png"))
                .unwrap();
        }
    }
}
