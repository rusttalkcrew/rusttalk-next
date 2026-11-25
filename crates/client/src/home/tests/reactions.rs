//! Reactions under a message: chips with counts, mine lit; a click toggles mine, and the
//! hover toolbar's Add reaction opens the emoji picker. Hovering a chip, or holding it,
//! shows who reacted, led by the same emoji picture as the chip.

use super::*;
use egui_kittest::kittest::Queryable;
use rusttalk_core::messages::ReactionEmoji;

const TARGET: Id = [40; 32];
/// A server emoji, with a picture.
const PARROT: Id = [41; 32];

fn text(id: Id, person: Id, at: i64, text: &str) -> Msg {
    Msg {
        backfilled: false,
        id,
        person,
        at,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        body: Body::Text(text.into()),
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: Vec::new(),
        copy_files: Vec::new(),
    }
}

fn reaction(record: u8, person: Id, target: Id, emoji: &str, on: bool) -> Msg {
    keyed_reaction(
        record,
        person,
        target,
        ReactionEmoji::Unicode(emoji.into()),
        on,
    )
}

fn keyed_reaction(record: u8, person: Id, target: Id, emoji: ReactionEmoji, on: bool) -> Msg {
    let key = emoji.key();
    Msg {
        body: Body::PollPart(reaction_vote(&target, &key, u64::from(record), on)),
        ..text([record; 32], person, 1_700_000_100 + i64::from(record), "")
    }
}

/// The votes sent since the last call: poll ID and choices (their clock is the click's).
fn sent_votes(sent: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> Vec<(Id, Vec<u8>)> {
    std::iter::from_fn(|| sent.try_recv().ok())
        .filter_map(|command| match command {
            Command::Send(draft) => match draft.poll.map(|part| *part) {
                Some(PollPart::Vote { poll, choices, .. }) => Some((poll, choices)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn expected(emoji: &str, on: bool) -> (Id, Vec<u8>) {
    let key = ReactionEmoji::Unicode(emoji.into()).key();
    match reaction_vote(&TARGET, &key, 0, on) {
        PollPart::Vote { poll, choices, .. } => (poll, choices),
        _ => unreachable!(),
    }
}

fn who(names: &str, emoji: &str) -> String {
    let key = ReactionEmoji::Unicode(emoji.into()).key();
    let shown = crate::emoji::reacted(&key, &[])
        .expect("a picker emoji")
        .name();
    t!("home.reacted_with", names = names, emoji = shown)
}

fn harness(
    dark: bool,
    size: Vec2,
) -> (
    egui_kittest::Harness<'static, Option<Home>>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let (mut h, sent) = files_harness(dark);
    h.ctx.data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
    h.set_size(size);
    let server = &mut h.state_mut().as_mut().unwrap().servers[0];
    let green = [40, 200, 120, 255];
    let mut png = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(32, 32, image::Rgba(green))
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let image = ColorImage::from_rgba_unmultiplied([32, 32], &[green; 32 * 32].concat());
    server.dir.emoji.push(CustomEmoji {
        id: PARROT,
        name: "party_parrot".into(),
        sticker: false,
        pack: [0; 32],
        picture: Some(Picture::new(
            PARROT,
            vec![(image, 0)],
            Arc::new(png.into_inner()),
        )),
    });
    let chat = server.chats.get_mut(&PROJECTS).unwrap();
    chat.at_start = true;
    chat.merge(
        vec![text(TARGET, ANN, 1_700_000_000, "Pizza tonight?")],
        false,
    );
    chat.merge(
        vec![
            reaction(1, ANN, TARGET, "👍", true),
            reaction(2, ME, TARGET, "❤️", true),
            // Ann's heart was taken back; mine stays.
            reaction(3, ANN, TARGET, "❤️", true),
            reaction(4, ANN, TARGET, "❤️", false),
            // A reaction to a message that isn't loaded shows nowhere.
            reaction(5, ANN, [99; 32], "🎉", true),
            // Warm colours, to check mine stay readable on the accent tint.
            reaction(6, ME, TARGET, "🧡", true),
            reaction(7, ME, TARGET, "💛", true),
            reaction(8, ANN, TARGET, "🔥", true),
            reaction(9, ME, TARGET, "🔥", true),
            // The one from the bug report: drawn as a picture, not a font glyph.
            reaction(10, ANN, TARGET, "🆖", true),
            keyed_reaction(11, ANN, TARGET, ReactionEmoji::Custom(PARROT), true),
        ],
        false,
    );
    h.run_steps(4);
    (h, sent)
}

#[test]
fn clicking_a_reaction_toggles_mine() {
    for dark in [true, false] {
        for size in [Vec2::new(390., 844.), Vec2::new(1100., 700.)] {
            let (mut h, mut sent) = harness(dark, size);
            // Taken back, or about a message not loaded: not shown.
            assert!(h.query_by_label(&who("Ann", "🎉")).is_none());
            assert!(h.query_by_label(&who("Me, Ann", "❤️")).is_none());
            // My heart is on: a click takes it back.
            h.get_by_label(&who("Me", "❤️")).click();
            h.run_steps(2);
            assert_eq!(sent_votes(&mut sent), [expected("❤️", false)]);
            // Ann's thumbs up isn't mine: a click adds mine.
            h.get_by_label(&who("Ann", "👍")).click();
            h.run_steps(2);
            assert_eq!(sent_votes(&mut sent), [expected("👍", true)]);
        }
    }
}

/// The fill and outline colour painted exactly on a widget's rect.
fn chip_look(h: &egui_kittest::Harness<'_, Option<Home>>, rect: egui::Rect) -> (Color32, Color32) {
    (h.output().shapes.iter())
        .find_map(|clipped| match &clipped.shape {
            Shape::Rect(shape) if shape.rect == rect => Some((shape.fill, shape.stroke.color)),
            _ => None,
        })
        .expect("chip is painted")
}

/// How many times a server emoji's picture is painted.
fn picture_count(h: &egui_kittest::Harness<'_, Option<Home>>, key: Id) -> usize {
    let texture = h
        .ctx
        .data(|d| d.get_temp::<TextureHandle>(UiId::unique(("picture", key))));
    let Some(texture) = texture else {
        return 0;
    };
    (h.ctx.tessellate(h.output().shapes.clone(), h.output().pixels_per_point))
        .iter()
        .filter(|s| matches!(&s.primitive, egui::epaint::Primitive::Mesh(mesh) if mesh.texture_id == texture.id()))
        .count()
}

#[test]
fn hovering_a_chip_lights_it_and_shows_who_reacted_with_the_emoji_picture() {
    for dark in [true, false] {
        let (mut h, _sent) = harness(dark, Vec2::new(1100., 700.));
        // Ann's 🆖 and my ❤️ both light up; mine keeps its accent outline.
        for (emoji, names) in [("🆖", "Ann"), ("❤️", "Me")] {
            let rect = h.get_by_label(&who(names, emoji)).rect();
            let idle = chip_look(&h, rect);
            let tip = t!("home.reacted", names = names);
            assert!(h.query_by_label(&tip).is_none());
            h.hover_at(rect.center());
            h.run_steps(4);
            let hovered = chip_look(&h, rect);
            assert_ne!(idle.0, hovered.0, "{emoji}: the fill lights up");
            assert_eq!(
                h.output().platform_output.cursor_icon,
                egui::CursorIcon::PointingHand
            );
            if names == "Me" {
                assert_eq!(idle.1, hovered.1, "mine keeps its accent outline");
            }
            // The tip: the emoji in the emoji font (drawn from its picture), then who.
            h.get_by_label(&tip);
            h.get_by_label(emoji);
            h.hover_at(egui::pos2(5.0, 5.0));
            h.run_steps(4);
        }
        // A server emoji's tip shows its picture: once in the chip, once in the tip.
        let label = t!("home.reacted_with", names = "Ann", emoji = ":party_parrot:");
        let rect = h.get_by_label(&label).rect();
        assert_eq!(picture_count(&h, PARROT), 1);
        h.hover_at(rect.center());
        h.run_steps(4);
        h.get_by_label(&t!("home.reacted", names = "Ann"));
        assert_eq!(picture_count(&h, PARROT), 2);
    }
}

#[test]
fn holding_a_chip_shows_who_reacted_without_toggling_it() {
    use eframe::egui::TouchPhase;
    let (mut h, mut sent) = harness(true, Vec2::new(390., 844.));
    let pos = h.get_by_label(&who("Ann", "👍")).rect().center();
    super::touch::finger(&mut h, TouchPhase::Start, pos);
    h.run_steps(8);
    super::touch::finger(&mut h, TouchPhase::End, pos);
    h.run_steps(2);
    h.get_by_label(&t!("home.reacted", names = "Ann"));
    h.get_by_label("👍");
    assert!(sent_votes(&mut sent).is_empty());
}

#[test]
fn add_reaction_picks_an_emoji_from_the_picker() {
    let (mut h, mut sent) = harness(true, Vec2::new(1100., 700.));
    let row = h.get_by_label("Pizza tonight?").rect().center();
    h.hover_at(row);
    h.run_steps(3);
    // A reaction is an emoji: the picker adds no sticker tab.
    let stickers = crate::emoji::picker::Tab::Stickers.label();
    let before = h.query_all_by_label(stickers).count();
    h.get_by_label(t!("home.add_reaction")).click();
    h.run_steps(4);
    assert_eq!(h.query_all_by_label(stickers).count(), before);
    let first = &crate::emoji::PICKER[0];
    h.get_all_by_label(first.label).next().unwrap().click();
    h.run_steps(3);
    assert_eq!(sent_votes(&mut sent), [expected(first.sequence, true)]);
    let chat = &h.state().as_ref().unwrap().servers[0].chats[&PROJECTS];
    assert!(chat.reacting.is_none());
}

#[test]
fn reaction_screenshots() {
    let Ok(dir) = std::env::var("RUSTTALK_REACTION_SCREENSHOTS") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for (theme, dark) in [("dark", true), ("light", false)] {
        for (tag, size) in [
            ("desktop-1100x700", Vec2::new(1100.0, 700.0)),
            ("phone-390x844", Vec2::new(390.0, 844.0)),
        ] {
            let (mut h, _sent) = harness(dark, size);
            let row = h.get_by_label("Pizza tonight?").rect().center();
            h.hover_at(row);
            h.run_steps(4);
            h.render()
                .unwrap()
                .save(format!("{dir}/{tag}-{theme}.png"))
                .unwrap();
            for (emoji, file) in [("🆖", "tooltip"), (":party_parrot:", "tooltip-custom")] {
                let label = t!("home.reacted_with", names = "Ann", emoji = emoji);
                let chip = h.get_by_label(&label).rect().center();
                h.hover_at(chip);
                h.run_steps(6);
                h.render()
                    .unwrap()
                    .save(format!("{dir}/{tag}-{theme}-{file}.png"))
                    .unwrap();
            }
            h.hover_at(row);
            h.run_steps(4);
            h.get_by_label(t!("home.add_reaction")).click();
            h.run_steps(6);
            h.render()
                .unwrap()
                .save(format!("{dir}/{tag}-{theme}-picker.png"))
                .unwrap();
        }
    }
}
