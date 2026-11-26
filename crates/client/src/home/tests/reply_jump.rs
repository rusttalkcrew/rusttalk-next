//! The line above a reply jumps to the message it answers (#639): scrolled to and
//! briefly highlighted, paged in from older history when needed, and a short notice
//! when it is gone.

use super::*;
use crate::net::Update;
use egui_kittest::{Harness, kittest::Queryable};
use std::path::PathBuf;

const JUMP: &str = "Jump to replied message";

fn id(n: u16) -> Id {
    let mut id = [4; 32];
    id[..2].copy_from_slice(&n.to_be_bytes());
    id
}

fn author(n: u16) -> Id {
    if n.is_multiple_of(2) { ANN } else { ME }
}

fn line(n: u16) -> Msg {
    Msg {
        backfilled: false,
        id: id(n),
        person: author(n),
        at: 1_700_000_000 + i64::from(n) * 600,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        body: Body::Text(format!("Line L{n:04}.")),
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

/// Message `n` answering message `to`.
fn reply(n: u16, to: u16, text: &str) -> Msg {
    Msg {
        reply_to: Some(id(to)),
        quoted_author: Some(author(to)),
        body: Body::Text(text.into()),
        ..line(n)
    }
}

/// A phone-sized chat holding `messages`; `oldest` is set when older history is left.
fn harness(
    messages: Vec<Msg>,
    oldest: Option<u16>,
) -> (
    Harness<'static, Option<Home>>,
    tokio::sync::mpsc::UnboundedReceiver<Command>,
) {
    let (mut h, mut sent) = files_harness(true);
    h.set_size(Vec2::new(390.0, 844.0));
    h.ctx.data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
    let chat = chat_mut(&mut h);
    chat.at_start = oldest.is_none();
    chat.oldest = oldest.map(id);
    chat.merge(messages, false);
    h.run_steps(6);
    while sent.try_recv().is_ok() {}
    (h, sent)
}

fn chat<'h>(h: &'h Harness<'static, Option<Home>>) -> &'h Chat {
    &h.state().as_ref().unwrap().servers[0].chats[&PROJECTS]
}

fn chat_mut<'h>(h: &'h mut Harness<'static, Option<Home>>) -> &'h mut Chat {
    (h.state_mut().as_mut().unwrap().servers[0].chats)
        .get_mut(&PROJECTS)
        .unwrap()
}

/// Where `marker` shows in full view, above the composer, if it does.
fn in_view(h: &Harness<'static, Option<Home>>, marker: &str) -> Vec<Rect> {
    let bottom = h.ctx.content_rect().bottom() - 60.0;
    (h.query_all_by_label_contains(marker))
        .map(|node| node.rect())
        .filter(|rect| rect.top() > 40.0 && rect.bottom() < bottom)
        .collect()
}

fn older_page(h: &mut Harness<'static, Option<Home>>, items: Vec<Msg>, oldest: u16, start: bool) {
    let ctx = h.ctx.clone();
    let home = h.state_mut().as_mut().unwrap();
    let update = Update::Messages {
        server: home.servers[0].address.clone(),
        channel: PROJECTS,
        items,
        older: true,
        page: Some((start, Some(id(oldest)))),
    };
    home.apply(&ctx, update, 1.0);
}

fn asked_older(sent: &mut tokio::sync::mpsc::UnboundedReceiver<Command>) -> bool {
    let mut asked = false;
    while let Ok(command) = sent.try_recv() {
        asked |= matches!(command, Command::Older(PROJECTS, _));
    }
    asked
}

#[test]
fn clicking_a_reply_scrolls_to_and_highlights_the_message() {
    let mut messages = lines(1..=120);
    messages.push(reply(121, 5, "Answer to five"));
    let (mut h, mut sent) = harness(messages, None);
    // Only the reply's preview mentions it; the message itself is far above.
    assert!(in_view(&h, "L0005.").len() == 1);
    assert!(chat(&h).flash.is_none());

    h.get_by_label(JUMP).click();
    h.run_steps(2);
    assert_eq!(chat(&h).flash.map(|f| (f.0, f.1)), Some((author(5), id(5))));
    assert!(chat(&h).seek.is_none());
    assert!(
        !asked_older(&mut sent),
        "loaded already: nothing to ask for"
    );
    // The message is in view now and the reply is not; it stays put.
    h.run_steps(2);
    let shown = in_view(&h, "Line L0005.");
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(in_view(&h, "Answer to five").is_empty());

    // The highlight fades out after a moment.
    h.run_steps(5);
    assert!(chat(&h).flash.is_none());
    assert_eq!(in_view(&h, "Line L0005."), shown);
}

#[test]
fn a_reply_to_an_unloaded_message_pages_in_history_until_found() {
    let mut messages = lines(101..=160);
    messages.push(reply(161, 5, "Answer to five"));
    let (mut h, mut sent) = harness(messages, Some(101));

    h.get_by_label(JUMP).click();
    h.run_steps(2);
    assert!(
        asked_older(&mut sent),
        "asks for the page before the oldest"
    );
    assert!(chat(&h).seek.is_some());

    // Not in this page: it keeps looking.
    older_page(&mut h, lines(51..=100), 51, false);
    h.run_steps(2);
    assert!(asked_older(&mut sent));
    assert!(chat(&h).seek.is_some());

    older_page(&mut h, lines(1..=50), 1, true);
    h.run_steps(3);
    assert!(chat(&h).seek.is_none());
    assert_eq!(chat(&h).flash.map(|f| (f.0, f.1)), Some((author(5), id(5))));
    assert!(!in_view(&h, "Line L0005.").is_empty());
    assert!(h.query_by_label(t!("home.reply_gone")).is_none());
}

#[test]
fn a_reply_to_a_deleted_message_says_so() {
    let mut messages = lines(101..=160);
    messages.push(reply(161, 5, "Answer to five"));
    let (mut h, mut sent) = harness(messages, Some(101));

    h.get_by_label(JUMP).click();
    h.run_steps(2);
    assert!(asked_older(&mut sent));
    // History ends without it.
    let mut page = lines(1..=100);
    page.retain(|msg| msg.id != id(5));
    older_page(&mut h, page, 1, true);
    h.run_steps(3);
    assert!(chat(&h).seek.is_none());
    assert!(chat(&h).flash.is_none());
    assert!(h.query_by_label(t!("home.reply_gone")).is_some());
}

#[test]
fn a_reply_beyond_the_pages_looked_through_says_so() {
    let mut messages = lines(101..=160);
    messages.push(reply(161, 5, "Answer to five"));
    let (mut h, mut sent) = harness(messages, Some(101));
    h.get_by_label(JUMP).click();
    h.run_steps(2);
    assert!(asked_older(&mut sent));
    // The last page it may ask for; older history is left but it stops.
    chat_mut(&mut h).seek.as_mut().unwrap().2 = 0;
    older_page(&mut h, lines(51..=100), 51, false);
    h.run_steps(2);
    assert!(!asked_older(&mut sent));
    assert!(chat(&h).seek.is_none());
    assert!(h.query_by_label(t!("home.reply_gone")).is_some());
}

/// Screenshots for the pull request: set `RUSTTALK_REPLY_SCREENSHOTS` to a folder.
#[test]
fn reply_line_screenshots() {
    let Some(dir) = std::env::var_os("RUSTTALK_REPLY_SCREENSHOTS") else {
        return;
    };
    std::fs::create_dir_all(&dir).unwrap();
    let texts = [
        "Who's bringing snacks on Saturday?",
        "I can do chips and something sweet.",
        "Great! The trip starts at 9, meet at the station by the big clock so nobody gets lost on the way.",
        "Should we take the early train?",
        "Yes, it's quieter.",
        "Then I'll book the seats tonight.",
    ];
    let mut messages: Vec<Msg> = (1..=30)
        .map(|n| Msg {
            body: Body::Text(texts[usize::from(n) % texts.len()].into()),
            ..line(n)
        })
        .collect();
    messages.push(reply(31, 26, "Perfect, see you there!"));
    messages.push(reply(32, 4, "Chips are fine by me 😄"));
    for dark in [true, false] {
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            let (mut h, _sent) = harness(messages.clone(), None);
            h.ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            h.set_size(size);
            h.run_steps(6);
            let theme = if dark { "dark" } else { "light" };
            let save = |h: &mut Harness<'static, Option<Home>>, what: &str| {
                let name = format!("{what}-{}x{}-{theme}.png", size.x, size.y);
                h.render()
                    .unwrap()
                    .save(PathBuf::from(&dir).join(name))
                    .unwrap();
            };
            h.query_all_by_label(JUMP).next().unwrap().hover();
            h.run_steps(2);
            save(&mut h, "reply-lines");
            h.query_all_by_label(JUMP).last().unwrap().click();
            h.run_steps(4);
            save(&mut h, "jumped");
        }
    }
}
