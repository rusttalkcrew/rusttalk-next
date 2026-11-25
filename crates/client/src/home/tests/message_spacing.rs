//! A message group's name sits at the avatar's top and its first line right under it,
//! the same with or without mention chips (#592).

use super::*;
use egui_kittest::kittest::Queryable;

const JAKOB: Id = [8; 32];
const PHIL: Id = [9; 32];

const PLAIN: &str = "Bruh";
const MENTIONS_TAIL: &str = "Update v0.4.0 is being build rn.";
const FOLLOW_UP: &str = "Emoji 💪, a link https://example.org and `inline code`";
const REPLY: &str = "A reply";
const LINES: &str = "First line\nSecond line\n```rust\nfn main() {}\n```";

fn text(id: u8, person: Id, at: i64, text: &str) -> Msg {
    Msg {
        backfilled: false,
        id: [id; 32],
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

fn token(person: Id) -> String {
    format!("<@{}>", rusttalk_core::auth::encode(&person))
}

fn harness(dark: bool, size: Vec2) -> egui_kittest::Harness<'static, Option<Home>> {
    let (mut h, _sent) = files_harness(dark);
    h.ctx.data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
    h.set_size(size);
    let server = &mut h.state_mut().as_mut().unwrap().servers[0];
    let mut members = server.dir.members.to_vec();
    for (person, name) in [(JAKOB, "Jakob"), (PHIL, "Phil")] {
        let mut member = members[1].clone();
        member.person = person;
        member.name = Some(name.into());
        members.push(member);
    }
    server.dir.members = members.into();
    let chat = server.chats.get_mut(&PROJECTS).unwrap();
    chat.at_start = true;
    // The reported case: a message opening with two mention chips.
    let mentions = format!("{} {} {MENTIONS_TAIL}", token(JAKOB), token(PHIL));
    let mut reply = text(4, ANN, 1_700_000_300, REPLY);
    reply.reply_to = Some([3; 32]);
    reply.quoted_author = Some(ME);
    let joined = Msg {
        body: Body::Notice {
            name: "Phil".into(),
            action: "joined",
            detail: "fingerprint".into(),
        },
        ..text(5, PHIL, 1_700_000_400, "")
    };
    chat.merge(
        vec![
            text(1, ANN, 1_700_000_000, PLAIN),
            text(2, ME, 1_700_000_100, &mentions),
            text(3, ME, 1_700_000_110, FOLLOW_UP),
            reply,
            joined,
            text(6, PHIL, 1_700_000_500, LINES),
        ],
        false,
    );
    h.run_steps(4);
    h
}

/// A sender's `nth` avatar and name, and the first line of their message below it.
fn rects(
    h: &egui_kittest::Harness<'static, Option<Home>>,
    name: &str,
    nth: usize,
    line: &str,
) -> [Rect; 3] {
    let (avatars, labels): (Vec<Rect>, Vec<Rect>) = (h.query_all_by_label(name))
        .map(|node| node.rect())
        .partition(|rect| (rect.width() - MESSAGE_AVATAR).abs() < 0.5);
    let label = labels[nth];
    let text = (h.query_all_by_label_contains(line))
        .map(|node| node.rect())
        .find(|rect| rect.top() > label.top())
        .unwrap();
    [avatars[nth], label, text]
}

#[test]
fn name_sits_at_the_avatar_top_and_text_right_below() {
    for size in [Vec2::new(390., 844.), Vec2::new(1100., 700.)] {
        let h = harness(true, size);
        let gaps = [
            ("Ann", 0, PLAIN),
            ("Me", 0, MENTIONS_TAIL),
            ("Ann", 1, REPLY),
        ]
        .map(|(name, nth, line)| {
            let [avatar, label, text] = rects(&h, name, nth, line);
            assert!(
                (label.top() - avatar.top()).abs() < 1.0,
                "{name}: name {label:?}, avatar {avatar:?}"
            );
            text.top() - label.bottom()
        });
        // Mention chips (or a quoted reply) don't push the text down.
        for gap in gaps {
            assert!((gap - gaps[0]).abs() < 1.0, "{gaps:?}");
            assert!(gap <= ui::SPACE_XS + 0.5, "{gaps:?}");
        }
    }
}

#[test]
fn message_spacing_screenshots() {
    let Ok(dir) = std::env::var("RUSTTALK_SPACING_SCREENSHOTS") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    for (theme, dark) in [("dark", true), ("light", false)] {
        for (tag, size) in [
            ("desktop-1100x700", Vec2::new(1100.0, 700.0)),
            ("phone-390x844", Vec2::new(390.0, 844.0)),
        ] {
            let mut h = harness(dark, size);
            h.run_steps(2);
            h.render()
                .unwrap()
                .save(format!("{dir}/{tag}-{theme}.png"))
                .unwrap();
        }
    }
}
