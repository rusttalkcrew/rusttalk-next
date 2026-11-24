//! A long chat (#461): only the rows near the view are laid out each frame, and
//! scrolling, sticking to the newest message and loading older pages still behave.

use super::*;
use crate::net::Update;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use std::path::PathBuf;

fn id(n: u16) -> Id {
    let mut id = [1; 32];
    id[..2].copy_from_slice(&n.to_be_bytes());
    id
}

fn author(n: u16) -> Id {
    if (n / 3).is_multiple_of(2) { ANN } else { ME }
}

/// Message `n`, with "L0042."-style markers to find it by. Runs of three share an
/// author; some wrap, some have two paragraphs and some reply to an older one.
fn line(n: u16) -> Msg {
    let text = match n % 5 {
        0 => format!(
            "Line L{n:04}. A longer message that wraps onto another line on a phone, with a few more words to be sure it does."
        ),
        3 => format!("Line L{n:04}.\n\nWith a second paragraph"),
        _ => format!("Line L{n:04}."),
    };
    let reply = (n.is_multiple_of(7) && n > 10).then(|| n - 9);
    Msg {
        backfilled: false,
        id: id(n),
        person: author(n),
        at: 1_700_000_000 + i64::from(n) * 30,
        reply_to: reply.map(id),
        quoted_author: reply.map(author),
        edit_of: None,
        body: Body::Text(text),
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: Vec::new(),
        copy_files: Vec::new(),
    }
}

fn lines(range: std::ops::RangeInclusive<u16>) -> Vec<Msg> {
    range.map(line).collect()
}

fn long_chat(dark: bool, size: Vec2, messages: Vec<Msg>) -> Harness<'static, Option<Home>> {
    let (mut h, _sent) = files_harness(dark);
    h.set_size(size);
    h.ctx.data_mut(|d| d.remove::<bool>(files_id(PROJECTS)));
    {
        let home = h.state_mut().as_mut().unwrap();
        let chat = home.servers[0].chats.get_mut(&PROJECTS).unwrap();
        chat.at_start = true;
        chat.merge(messages, false);
    }
    h.run_steps(6);
    h
}

/// The same scene before and after virtualizing, to compare by eye.
#[test]
fn long_chat_looks_the_same_at_the_bottom() {
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let mut h = long_chat(dark, size, lines(1..=2000));
            assert!(h.query_by_label_contains("L2000.").is_some());
            if let Some(dir) = std::env::var_os("RUSTTALK_LONG_CHAT_SCREENSHOTS") {
                std::fs::create_dir_all(&dir).unwrap();
                let theme = if dark { "dark" } else { "light" };
                let name = format!("bottom-{}x{}-{theme}.png", size.x, size.y);
                h.render()
                    .unwrap()
                    .save(PathBuf::from(dir).join(name))
                    .unwrap();
            }
        }
    }
}

fn chat<'h>(h: &'h Harness<'static, Option<Home>>) -> &'h Chat {
    &h.state().as_ref().unwrap().servers[0].chats[&PROJECTS]
}

fn chat_mut<'h>(h: &'h mut Harness<'static, Option<Home>>) -> &'h mut Chat {
    (h.state_mut().as_mut().unwrap().servers[0].chats)
        .get_mut(&PROJECTS)
        .unwrap()
}

/// The message lines in view, top first, with where they are.
fn on_screen(h: &Harness<'static, Option<Home>>) -> Vec<(String, egui::Rect)> {
    let bottom = h.ctx.content_rect().bottom() - 60.0;
    let mut lines: Vec<_> = (h.query_all_by_label_contains("Line L"))
        // A label keeps its text as its value.
        .filter_map(|node| {
            let text = node.accesskit_node();
            Some((text.value().or(text.label())?.to_string(), node.rect()))
        })
        .filter(|(_, rect)| rect.top() > 50.0 && rect.bottom() < bottom)
        .collect();
    lines.sort_by(|a, b| a.1.top().total_cmp(&b.1.top()));
    lines
}

fn rect_of(h: &Harness<'static, Option<Home>>, marker: &str) -> Option<egui::Rect> {
    h.query_by_label_contains(marker).map(|node| node.rect())
}

fn update(h: &mut Harness<'static, Option<Home>>, items: Vec<Msg>, older: bool) {
    let ctx = h.ctx.clone();
    let home = h.state_mut().as_mut().unwrap();
    let update = Update::Messages {
        server: home.servers[0].address.clone(),
        channel: PROJECTS,
        items,
        older,
        page: older.then_some((true, Some(id(1)))),
    };
    home.apply(&ctx, update, 1.0);
    h.run_steps(6);
}

#[test]
fn a_long_chat_lays_out_only_the_rows_near_the_view() {
    for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
        let mut h = long_chat(true, size, lines(1..=2000));
        let drawn = chat(&h).rows.drawn;
        assert!((1..=80).contains(&drawn), "{drawn} rows laid out");
        let nodes = h.query_all_by_label_contains("Line L").count();
        // A row's text, and the line it quotes when it is a reply.
        assert!(nodes <= 2 * drawn, "{nodes} message nodes for {drawn} rows");
        assert!(h.query_by_label_contains("L0001.").is_none());
        // The newest message is in full view, above the composer.
        let newest = rect_of(&h, "L2000.").expect("newest message");
        assert!(newest.bottom() <= size.y - 40.0, "{newest:?}");
        // Settled: the next frame lays out the same.
        h.step();
        assert_eq!(rect_of(&h, "L2000."), Some(newest));
        assert_eq!(chat(&h).rows.drawn, drawn);
    }
}

#[test]
fn new_messages_stick_to_the_bottom_and_leave_a_scrolled_up_view_alone() {
    let size = Vec2::new(390.0, 844.0);
    let mut h = long_chat(false, size, lines(1..=2000));
    update(&mut h, vec![line(2001)], false);
    let newest = rect_of(&h, "L2001.").expect("new message in view");
    assert!(newest.bottom() <= size.y - 40.0, "{newest:?}");

    // Scrolled up, a new message changes nothing in view.
    let offset = chat(&h).scroll_offset;
    chat_mut(&mut h).scroll_to = Some(offset - 3000.0);
    h.run_steps(4);
    let before = on_screen(&h);
    assert!(
        !before.is_empty(),
        "nothing in view at {}",
        chat(&h).scroll_offset
    );
    update(&mut h, vec![line(2002)], false);
    assert_eq!(on_screen(&h), before);
    assert!(rect_of(&h, "L2002.").is_none());
}

#[test]
fn an_older_page_keeps_the_view_in_place_and_the_top_is_reachable() {
    for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
        let (mut h, _sent) = files_harness(true);
        h.set_size(size);
        h.ctx.data_mut(|d| d.remove::<bool>(files_id(PROJECTS)));
        chat_mut(&mut h).merge(lines(1001..=2000), false);
        h.run_steps(6);
        // Up a bit from the bottom, as when reading back.
        let offset = chat(&h).scroll_offset;
        chat_mut(&mut h).scroll_to = Some(offset - 2000.0);
        h.run_steps(4);
        let before = on_screen(&h);
        let (label, rect) = before.first().cloned().expect("a message in view");

        update(&mut h, lines(1..=1000), true);
        let after = on_screen(&h);
        let moved = (after.iter()).find(|(l, _)| *l == label).map(|(_, r)| *r);
        assert_eq!(moved, Some(rect), "{label} stays where it was: {after:?}");
        assert_eq!(after, before);

        // All the way up: the first rows are laid out where the view is, with
        // replies quoting what they answer.
        chat_mut(&mut h).scroll_to = Some(0.0);
        h.run_steps(6);
        let first = rect_of(&h, "L0001.").expect("the first message");
        assert!(first.top() > 0.0 && first.bottom() < size.y, "{first:?}");
        assert!(h.query_by_label_contains("Me: Line L0005.").is_some());
        assert!(chat(&h).rows.drawn <= 80);
    }
}
