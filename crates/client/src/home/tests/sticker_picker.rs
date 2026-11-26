//! The sticker picker (#641): tabs across the top, a grid of four, and always-playing
//! stickers. On a phone it docks under the composer and a tapped sticker waits large
//! above it until tapped again or sent; on a desktop a click sends it.

use super::*;
use egui_kittest::{Harness, kittest::Queryable};

const PARTY: Id = [7; 32];
const ANIMALS: Id = [9; 32];
const HUG: Id = [21; 32];

const PHONE: Vec2 = Vec2::new(390.0, 844.0);
const DESKTOP: Vec2 = Vec2::new(1100.0, 700.0);

/// A synthetic round face of `color`; each frame moves its eyes, so frames differ.
fn face(key: u8, color: [u8; 3], frames: u32) -> Picture {
    let side = 96usize;
    let frame = |at: u32| {
        let mut pixels = vec![Color32::TRANSPARENT; side * side];
        let look = at as f32 * 6.0 - 6.0;
        for y in 0..side {
            for x in 0..side {
                let (fx, fy) = (x as f32 - 48.0, y as f32 - 48.0);
                let eye = |ex: f32| (fx - ex - look).hypot(fy + 10.0) < 7.0;
                pixels[y * side + x] = if fx.hypot(fy) > 44.0 {
                    Color32::TRANSPARENT
                } else if eye(-16.0) || eye(16.0) || (fy > 14.0 && fy < 20.0 && fx.abs() < 18.0) {
                    Color32::from_rgb(40, 30, 30)
                } else {
                    Color32::from_rgb(color[0], color[1], color[2])
                };
            }
        }
        (ColorImage::new([side; 2], pixels), 120)
    };
    Picture::new([key; 32], (0..frames).map(frame).collect(), Arc::default())
}

fn sticker(id: u8, name: &str, pack: Id, picture: Picture) -> CustomEmoji {
    CustomEmoji {
        id: [id; 32],
        name: name.into(),
        sticker: true,
        pack,
        picture: Some(picture),
    }
}

/// The projects channel with two sticker packs, "Party" (one sticker animated) and
/// "Animals". All pictures are drawn here.
fn harness(
    dark: bool,
    size: Vec2,
) -> (
    Harness<'static, Option<Home>>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let (mut h, sent) = files_harness(dark);
    h.ctx.data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
    h.set_size(size);
    let dir = &mut h.state_mut().as_mut().unwrap().servers[0].dir;
    dir.packs = vec![
        crate::server::EmojiPack {
            id: PARTY,
            name: Some("Party".into()),
        },
        crate::server::EmojiPack {
            id: ANIMALS,
            name: Some("Animals".into()),
        },
    ];
    dir.emoji = vec![
        sticker(21, "big_hug", PARTY, face(21, [250, 190, 60], 3)),
        sticker(22, "cheers", PARTY, face(22, [120, 200, 250], 1)),
        sticker(23, "high_five", PARTY, face(23, [250, 140, 170], 1)),
        sticker(24, "yay", PARTY, face(24, [160, 230, 130], 1)),
        sticker(25, "dance", PARTY, face(25, [200, 160, 250], 1)),
        sticker(31, "cat_nap", ANIMALS, face(31, [240, 170, 110], 1)),
        sticker(32, "puppy", ANIMALS, face(32, [210, 190, 160], 1)),
    ];
    h.run_steps(4);
    (h, sent)
}

/// The stickers sent since last asked, by ID.
fn sent_stickers(sent: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<Option<Id>> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Send(draft) => Some(draft.sticker),
            _ => None,
        })
        .collect()
}

fn open_stickers(h: &mut Harness<'static, Option<Home>>) {
    h.get_by_label(t!("home.sticker_picker")).click();
    h.run_steps(4);
}

fn preview_label(name: &str) -> String {
    t!("picker.send_sticker", name = format!(":{name}:"))
}

#[test]
fn tabs_switch_between_packs() {
    for size in [PHONE, DESKTOP] {
        let (mut h, _sent) = harness(true, size);
        open_stickers(&mut h);
        assert!(h.query_by_label(":big_hug:").is_some());
        assert!(h.query_by_label(":cat_nap:").is_none());
        h.get_by_label("Animals").click();
        h.run_steps(3);
        assert!(h.query_by_label(":cat_nap:").is_some());
        assert!(h.query_by_label(":big_hug:").is_none());
        h.get_by_label("Party").click();
        h.run_steps(3);
        assert!(h.query_by_label(":big_hug:").is_some());
    }
}

#[test]
fn phone_tap_previews_and_a_second_tap_sends() {
    let (mut h, mut sent) = harness(true, PHONE);
    open_stickers(&mut h);
    h.get_by_label(":big_hug:").click();
    h.run_steps(3);
    assert!(sent_stickers(&mut sent).is_empty(), "a tap only previews");
    // × puts it away without sending.
    h.get_by_label(t!("picker.close_preview")).click();
    h.run_steps(3);
    assert!(h.query_by_label(&preview_label("big_hug")).is_none());
    assert!(sent_stickers(&mut sent).is_empty());

    h.get_by_label(":big_hug:").click();
    h.run_steps(3);
    h.get_by_label(&preview_label("big_hug")).click();
    h.run_steps(3);
    assert_eq!(sent_stickers(&mut sent), [Some(HUG)]);
    assert!(h.query_by_label(&preview_label("big_hug")).is_none());
    // The panel stays up for the next one, and the sticker is now recent.
    h.get_by_label(t!("picker.recent"));

    // Send sends the previewed sticker too.
    h.get_by_label("Animals").click();
    h.run_steps(3);
    h.get_by_label(":cat_nap:").click();
    h.run_steps(3);
    h.get_by_label(t!("home.send")).click();
    h.run_steps(3);
    assert_eq!(sent_stickers(&mut sent), [Some([31; 32])]);
}

#[test]
fn desktop_click_sends_directly() {
    let (mut h, mut sent) = harness(false, DESKTOP);
    open_stickers(&mut h);
    h.get_by_label(":big_hug:").click();
    h.run_steps(3);
    assert_eq!(sent_stickers(&mut sent), [Some(HUG)]);
    assert!(h.query_by_label(&preview_label("big_hug")).is_none());
}

/// The textures painted this frame, and the repaint asked for.
fn painted(
    h: &mut Harness<'static, Option<Home>>,
    seconds: f64,
) -> (Vec<egui::TextureId>, Duration) {
    h.input_mut().time = Some(seconds);
    h.step();
    let textures = (h.output().shapes.iter())
        .filter_map(|shape| match &shape.shape {
            egui::epaint::Shape::Rect(rect) => rect.brush.as_ref().map(|b| b.fill_texture_id),
            _ => None,
        })
        .collect();
    let delay = h.output().viewport_output[&egui::ViewportId::ROOT].repaint_delay;
    (textures, delay)
}

#[test]
fn stickers_play_without_hover() {
    for size in [PHONE, DESKTOP] {
        let (mut h, _sent) = harness(true, size);
        open_stickers(&mut h);
        // The pointer leaves the picker: nothing is hovered.
        h.input_mut().events.push(egui::Event::PointerGone);
        let (first, _) = painted(&mut h, 10.0);
        let (second, delay) = painted(&mut h, 10.13);
        assert_ne!(first, second, "the animated sticker moved on a frame");
        assert!(delay <= Duration::from_millis(120), "{delay:?}");
        let (third, _) = painted(&mut h, 10.26);
        assert!(third != first && third != second);
    }
}

#[test]
fn picker_screenshots() {
    let Some(dir) = std::env::var_os("RUSTTALK_PICKER_SCREENSHOTS").map(PathBuf::from) else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for (theme, dark) in [("dark", true), ("light", false)] {
        for (tag, size) in [("phone-390x844", PHONE), ("desktop-1100x700", DESKTOP)] {
            let (mut h, _sent) = harness(dark, size);
            let shoot = |h: &mut Harness<'static, Option<Home>>, what: &str| {
                h.run_steps(4);
                (h.render().unwrap())
                    .save(dir.join(format!("{tag}-{theme}-{what}.png")))
                    .unwrap();
            };
            open_stickers(&mut h);
            shoot(&mut h, "stickers");
            if size == PHONE {
                h.get_by_label(":big_hug:").click();
                shoot(&mut h, "preview");
            }
            h.get_by_label(t!("picker.emoji")).click();
            shoot(&mut h, "emoji");
        }
    }
}

/// The message box.
fn message_box() -> egui_kittest::kittest::By<'static> {
    egui_kittest::kittest::by().role(egui::accesskit::Role::MultilineTextInput)
}

fn box_focused(h: &Harness<'static, Option<Home>>) -> bool {
    h.ctx.memory(|m| m.has_focus(composer_id(PROJECTS)))
}

/// A spot in the (empty) chat, above the composer.
const CHAT: egui::Pos2 = egui::pos2(195.0, 120.0);

fn tap(h: &mut Harness<'static, Option<Home>>, pos: egui::Pos2) {
    use egui::TouchPhase;
    super::touch::finger(h, TouchPhase::Start, pos);
    super::touch::finger(h, TouchPhase::End, pos);
    h.run_steps(3);
}

/// Panel and keyboard are never up together on a phone (#653).
#[test]
fn phone_box_swaps_the_panel_for_the_keyboard() {
    let (mut h, _sent) = harness(true, PHONE);
    open_stickers(&mut h);
    assert!(h.query_by_label(":big_hug:").is_some());
    assert!(!box_focused(&h));
    // Finger down and up on separate frames, as on an iPhone.
    let field = h.get(message_box()).rect().center();
    tap(&mut h, field);
    assert!(h.query_by_label(":big_hug:").is_none(), "the panel closed");
    assert!(box_focused(&h), "the box has focus, so the keyboard is up");
    assert!(h.output().platform_output.ime.is_some());

    // Opening it again takes the focus, so the keyboard goes away.
    let button = h.get_by_label(t!("home.sticker_picker")).rect().center();
    tap(&mut h, button);
    assert!(h.query_by_label(":big_hug:").is_some());
    assert!(!box_focused(&h));
    assert!(
        h.output().platform_output.ime.is_none(),
        "no keyboard under the panel"
    );
}

#[test]
fn phone_tap_outside_closes_the_panel() {
    let (mut h, _sent) = harness(true, PHONE);
    open_stickers(&mut h);
    // A tap on a sticker previews it and the panel stays.
    h.get_by_label(":big_hug:").click();
    h.run_steps(3);
    assert!(h.query_by_label(":cheers:").is_some());
    tap(&mut h, CHAT);
    assert!(
        h.query_by_label(":big_hug:").is_none(),
        "a tap on the chat closes it"
    );
    assert!(h.query_by_label(&preview_label("big_hug")).is_none());
    assert!(!box_focused(&h));
}

#[test]
fn desktop_box_click_keeps_the_popover() {
    let (mut h, _sent) = harness(true, DESKTOP);
    open_stickers(&mut h);
    h.get(message_box()).click();
    h.run_steps(3);
    assert!(
        h.query_by_label(":big_hug:").is_some(),
        "the box keeps it open"
    );
    assert!(box_focused(&h));
    // Typing goes on with it open.
    h.get(message_box()).type_text("hi ");
    h.run_steps(2);
    assert!(h.query_by_label(":big_hug:").is_some());
    let pos = egui::pos2(300.0, 200.0);
    h.hover_at(pos);
    h.event(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    });
    h.event(egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Default::default(),
    });
    h.run_steps(3);
    assert!(
        h.query_by_label(":big_hug:").is_none(),
        "an outside click closes it"
    );
}

#[test]
fn keyboard_screenshots() {
    let Some(dir) = std::env::var_os("RUSTTALK_PICKER_KEYBOARD_SCREENSHOTS").map(PathBuf::from)
    else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for (theme, dark) in [("dark", true), ("light", false)] {
        let (mut h, _sent) = harness(dark, PHONE);
        super::touch::add_chat(&mut h);
        // Typing first, then the panel: it takes the keyboard's place.
        let field = h.get(message_box()).rect().center();
        tap(&mut h, field);
        h.get(message_box()).type_text("Party time ");
        h.run_steps(2);
        let button = h.get_by_label(t!("home.sticker_picker")).rect().center();
        tap(&mut h, button);
        h.run_steps(4);
        assert!(h.output().platform_output.ime.is_none());
        (h.render().unwrap())
            .save(dir.join(format!("phone-390x844-{theme}-panel-without-keyboard.png")))
            .unwrap();
    }
}
