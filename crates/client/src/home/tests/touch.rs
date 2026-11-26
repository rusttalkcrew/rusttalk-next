//! Real touch sequences include a primary pointer and lose hover on lift.
use super::*;
use eframe::egui::{Event, Modifiers, PointerButton, Pos2, TouchDeviceId, TouchId, TouchPhase};
use egui_kittest::{Harness, kittest::Queryable};

pub(super) fn finger<S>(h: &mut Harness<'_, S>, phase: TouchPhase, pos: Pos2) {
    let events = &mut h.input_mut().events;
    events.push(Event::Touch {
        device_id: TouchDeviceId(0),
        id: TouchId(0),
        phase,
        pos,
        force: None,
    });
    match phase {
        TouchPhase::Move => events.push(Event::PointerMoved(pos)),
        TouchPhase::Start | TouchPhase::End => {
            if phase == TouchPhase::Start {
                events.push(Event::PointerMoved(pos));
            }
            events.push(Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: phase == TouchPhase::Start,
                modifiers: Modifiers::NONE,
            });
            if phase == TouchPhase::End {
                events.push(Event::PointerGone);
            }
        }
        TouchPhase::Cancel => unreachable!(),
    }
    h.step();
}

#[test]
fn touching_message_text_distinguishes_scroll_tap_and_hold() {
    for dark in [true, false] {
        let (mut h, _sent) = files_harness(dark);
        h.set_size(Vec2::new(390.0, 640.0));
        h.ctx.data_mut(|d| d.remove::<bool>(files_id(PROJECTS)));
        let chat = h.state_mut().as_mut().unwrap().servers[0]
            .chats
            .get_mut(&PROJECTS)
            .unwrap();
        let message = Msg {
            id: [3; 32],
            person: ME,
            body: Body::Text("See you tomorrow".into()),
            at: 1,
            backfilled: false,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            files: vec![],
            live: None,
            sticker: None,
            replicas: vec![],
            copy_files: vec![],
        };
        for i in 0..40 {
            let mut row = message.clone();
            row.id = [i; 32];
            if i != 35 {
                row.body = Body::Text(format!("Message {i}"));
            }
            chat.messages.push(row);
        }
        h.run_steps(4);
        // The chat was opened by touch, so egui already offers drag-to-scroll.
        let corner = egui::pos2(2.0, 2.0);
        finger(&mut h, TouchPhase::Start, corner);
        finger(&mut h, TouchPhase::End, corner);
        h.step();
        let pos = h.get_by_label("See you tomorrow").rect().center();
        let moved = pos + egui::vec2(0.0, 32.0);
        finger(&mut h, TouchPhase::Start, pos);
        finger(&mut h, TouchPhase::Move, pos + egui::vec2(0.0, 12.0));
        finger(&mut h, TouchPhase::Move, moved);
        h.run_steps(8);
        finger(&mut h, TouchPhase::End, moved);
        h.step();
        assert!(h.query_by_label("Reply").is_none());
        assert!(
            h.query_by_label("Copy text").is_none(),
            "a drag opens no menu"
        );
        let after_scroll = h.get_by_label("See you tomorrow").rect().center();
        assert!(after_scroll.y > pos.y, "{pos:?} to {after_scroll:?}");
        let pos = after_scroll;

        // A tap shows neither the hover toolbar nor the menu.
        finger(&mut h, TouchPhase::Start, pos);
        finger(&mut h, TouchPhase::End, pos);
        h.run_steps(2);
        assert!(
            h.query_by_label("Reply").is_none(),
            "no toolbar after a tap"
        );
        assert!(h.query_by_label("Copy text").is_none());

        // A hold opens the message menu, whose items work by touch.
        finger(&mut h, TouchPhase::Start, pos);
        h.run_steps(8);
        finger(&mut h, TouchPhase::End, pos);
        h.step();
        h.get_by_label("Copy text");
        h.get_by_label("Edit");
        let reply = h.get_by_label("Reply").rect().center();
        finger(&mut h, TouchPhase::Start, reply);
        finger(&mut h, TouchPhase::End, reply);
        h.step();
        assert_eq!(
            h.state().as_ref().unwrap().servers[0].chats[&PROJECTS].reply_to,
            Some((ME, [35; 32]))
        );
    }
}

#[test]
fn tapping_a_participant_reveals_working_tile_controls() {
    let (mut fixture, _sent) = files_harness(true);
    let mut home = fixture.state_mut().take().unwrap();
    home.servers[0].voice.insert(
        PROJECTS,
        vec![crate::server::Peer {
            person: ANN,
            connecting: false,
            unknown: false,
            status: Default::default(),
            apps: vec![],
            screens: vec![],
            camera: None,
            terminal: None,
            diagram: None,
        }],
    );
    let mut h = Harness::builder()
        .with_size(Vec2::new(390., 400.))
        .with_step_dt(0.05)
        .build_ui_state(
            |ui, home: &mut Option<Home>| {
                if let Some(home) = home {
                    call::stage(ui, home, 0, PROJECTS);
                }
            },
            None,
        );
    ui::install(&h.ctx);
    *h.state_mut() = Some(home);
    h.run_steps(5);
    let pos = h
        .get_by_role_and_label(egui::Role::Button, "Ann")
        .rect()
        .center();
    finger(&mut h, TouchPhase::Start, pos);
    finger(&mut h, TouchPhase::End, pos);
    h.step();
    let focus = h.get_by_label(t!("call.focus")).rect().center();
    finger(&mut h, TouchPhase::Start, focus);
    finger(&mut h, TouchPhase::End, focus);
    h.run_steps(4);
    h.get_by_label(t!("call.unfocus"));
}

#[test]
fn holding_a_nested_file_opens_its_menu_instead_of_the_message_menu() {
    let mut h = Harness::new_ui(|ui| {
        let message = ui.scope_builder(UiBuilder::new().sense(Sense::CLICK), |ui| {
            let file = ui.button("Saved file");
            ui::context_menu(&file, |m| m.item("", "Open file"));
        });
        ui::context_menu(&message.response, |m| m.item("", "Reply"));
    });
    let pos = h.get_by_label("Saved file").rect().center();
    finger(&mut h, TouchPhase::Start, pos);
    h.run_steps(8);
    finger(&mut h, TouchPhase::End, pos);
    h.step();
    h.get_by_label("Open file");
    assert!(h.query_by_label("Reply").is_none());
}

/// A short chat between Ann and this person, made up for tests and screenshots.
pub(super) fn add_chat(h: &mut Harness<'static, Option<Home>>) {
    let chat = h.state_mut().as_mut().unwrap().servers[0]
        .chats
        .get_mut(&PROJECTS)
        .unwrap();
    let lines = [
        (ANN, "Morning! Did the new build land?"),
        (ME, "Yes, it is on the test phone now"),
        (ANN, "Great, I will try the stickers after lunch"),
        (ME, "See you tomorrow"),
    ];
    for (i, (person, text)) in lines.into_iter().enumerate() {
        chat.messages.push(Msg {
            id: [i as u8 + 1; 32],
            person,
            body: Body::Text(text.into()),
            at: 1_700_000_000 + i as i64 * 60,
            backfilled: false,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            files: vec![],
            live: None,
            sticker: None,
            replicas: vec![],
            copy_files: vec![],
        });
    }
    h.run_steps(4);
}

/// Whether the row of `text` has the hover fill behind it.
fn row_lit(h: &Harness<'static, Option<Home>>, text: &str) -> bool {
    let at = h.get_by_label(text).rect().center();
    let dark = h.ctx.global_style().visuals.dark_mode;
    let fill = ui::appearance(&h.ctx).palette(dark).hover_soft;
    (h.output().shapes.iter()).any(|clipped| match &clipped.shape {
        egui::Shape::Rect(rect) => rect.fill == fill && rect.rect.contains(at),
        _ => false,
    })
}

/// A long press keeps the message lit while its menu is open (#653).
#[test]
fn long_press_lights_the_row_while_its_menu_is_open() {
    for dark in [true, false] {
        let (mut h, _sent) = files_harness(dark);
        h.set_size(Vec2::new(390.0, 844.0));
        h.ctx.data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
        add_chat(&mut h);
        let text = "Yes, it is on the test phone now";
        let pos = h.get_by_label(text).rect().center();
        assert!(!row_lit(&h, text));
        finger(&mut h, TouchPhase::Start, pos);
        h.run_steps(8);
        finger(&mut h, TouchPhase::End, pos);
        h.run_steps(2);
        h.get_by_label("Copy text");
        assert!(row_lit(&h, text), "the pressed message is lit");
        assert!(!row_lit(&h, "See you tomorrow"), "only that one");
        if let Some(dir) = std::env::var_os("RUSTTALK_PICKER_KEYBOARD_SCREENSHOTS") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let theme = if dark { "dark" } else { "light" };
            (h.render().unwrap())
                .save(dir.join(format!("phone-390x844-{theme}-long-press-menu.png")))
                .unwrap();
        }
        // A tap elsewhere closes the menu and puts the row back.
        let empty = egui::pos2(380.0, 120.0);
        finger(&mut h, TouchPhase::Start, empty);
        finger(&mut h, TouchPhase::End, empty);
        h.run_steps(3);
        assert!(h.query_by_label("Copy text").is_none());
        assert!(!row_lit(&h, text));
    }
}
