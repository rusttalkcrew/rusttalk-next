//! Reactions under a message: chips with counts, mine lit; a click toggles mine, and the
//! hover toolbar's Add reaction opens the emoji picker.

use super::*;
use egui_kittest::kittest::Queryable;
use rusttalk_core::messages::ReactionEmoji;

const TARGET: Id = [40; 32];

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
    let key = ReactionEmoji::Unicode(emoji.into()).key();
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
    let chat = (h.state_mut().as_mut().unwrap().servers[0].chats)
        .get_mut(&PROJECTS)
        .unwrap();
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
            h.get_by_label(t!("home.add_reaction")).click();
            h.run_steps(6);
            h.render()
                .unwrap()
                .save(format!("{dir}/{tag}-{theme}-picker.png"))
                .unwrap();
        }
    }
}
