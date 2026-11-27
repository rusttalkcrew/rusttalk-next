//! A channel's or DM's gallery (#632): every picture and video shared in its chat, as
//! square tiles, newest first under month headers. It is built from the loaded
//! messages; reaching its end loads older pages just as scrolling up the chat does, so
//! servers see the same page requests as for reading and nothing else. Thumbnails are
//! the encrypted ones sent along with each picture, decrypted on this device.

use std::{collections::HashSet, ops::Range};

use chrono::{DateTime, Datelike, Local, NaiveDate};
use eframe::egui::{self, Id as UiId, Rect, ScrollArea, Ui, Vec2};

use super::{At, Home, empty, events::month_title, files_id, load_older, time};
use crate::{
    files::{self, MediaKind, Shot},
    server::{Chat, Id, Msg, Status},
    ui,
};

/// Tiles are about this wide; phones get three or four columns.
const TILE: f32 = 112.0;
const MIN_COLUMNS: usize = 3;
const GAP: f32 = 2.0;
const HEADER: f32 = 40.0;
/// Animations playing in the grid at once, besides the one under the pointer.
const PLAYING: usize = 4;

fn shown_id(key: Id) -> UiId {
    UiId::unique(("gallery-shown", key))
}

/// Whether a channel (by ID) or DM (by the other person) shows its gallery.
pub(super) fn shown(ctx: &egui::Context, key: Id) -> bool {
    ctx.data(|d| d.get_temp(shown_id(key))).unwrap_or(false)
}

pub(super) fn set_shown(ctx: &egui::Context, key: Id, on: bool) {
    ctx.data_mut(|d| {
        d.insert_temp(shown_id(key), on);
        // The gallery and the shared files take the chat's place in turn.
        if on {
            d.insert_temp(files_id(key), false);
        }
    });
}

/// The header button that shows or hides the gallery.
pub(super) fn button(ui: &mut Ui, key: Id) {
    let on = shown(ui.ctx(), key);
    let (tip, look) = if on {
        (t!("gallery.hide"), ui::Look::Plain)
    } else {
        (t!("gallery.show"), ui::Look::Ghost)
    };
    if ui::icon_button(ui, "🖼", tip, look, Vec2::new(30.0, 26.0)).clicked() {
        set_shown(ui.ctx(), key, !on);
    }
}

/// A file in the gallery: its message and place among the message's files.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Entry {
    pub msg: usize,
    pub file: usize,
    pub kind: MediaKind,
}

/// The gallery's files, newest first. A file shown again (a page channel's pictures
/// go along with each version) counts once, at its newest.
pub(super) fn entries(messages: &[Msg]) -> Vec<Entry> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (at, msg) in messages.iter().enumerate().rev() {
        for (i, file) in msg.files.iter().enumerate() {
            if let Some(kind) = files::media_kind(file)
                && seen.insert(file.file.id)
            {
                out.push(Entry {
                    msg: at,
                    file: i,
                    kind,
                });
            }
        }
    }
    out
}

fn month(at: i64) -> NaiveDate {
    let day = DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .with_timezone(&Local)
        .date_naive();
    day.with_day(1).unwrap_or(day)
}

/// A line of the grid: a month's title, or a row of tiles (entries).
#[derive(Debug, PartialEq)]
pub(super) enum Line {
    Month(NaiveDate),
    Tiles(Range<usize>),
}

/// The grid's lines for `columns` tiles per row: each month starts a new row.
pub(super) fn lines(messages: &[Msg], entries: &[Entry], columns: usize) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < entries.len() {
        let current = month(messages[entries[start].msg].at);
        let end = (entries[start..].iter())
            .position(|e| month(messages[e.msg].at) != current)
            .map_or(entries.len(), |n| start + n);
        out.push(Line::Month(current));
        let mut row = start;
        while row < end {
            let stop = (row + columns).min(end);
            out.push(Line::Tiles(row..stop));
            row = stop;
        }
        start = end;
    }
    out
}

/// The gallery in place of the chat. `key` is the channel, or the other person in a
/// DM; `jumps` is whether the chat can scroll to a message (not pages or forums).
pub(super) fn view(ui: &mut Ui, home: &mut Home, at: At, chat: &mut Chat, jumps: bool) {
    let key = match at {
        At::Channel(_, channel) => channel,
        At::Dm(peer) => peer,
    };
    if let Some((person, id)) = home.files.take_jump(key) {
        jump(ui, key, chat, person, id);
        return;
    }
    let holder = |home: &Home, msg: &Msg| match at {
        At::Channel(index, _) => Some(index),
        At::Dm(peer) => (msg.replicas.first())
            .and_then(|url| home.servers.iter().position(|s| &s.address == url))
            .or_else(|| home.dm_server(&peer)),
    };
    let name =
        |home: &Home, index: Option<usize>, person: &Id| match index.map(|i| &home.servers[i]) {
            Some(server) if server.member(person).is_some() => server.member_name(person),
            _ => home.peer_name(person),
        };
    let shot = |home: &Home, entry: &Entry| -> Option<Shot> {
        let msg = &chat.messages[entry.msg];
        let index = holder(home, msg)?;
        let server = &home.servers[index];
        Some(Shot {
            address: server.address.clone(),
            commands: server.commands.clone(),
            file: msg.files.get(entry.file)?.clone(),
            caption: t!(
                "gallery.shared_by",
                name = name(home, Some(index), &msg.person),
                time = time(msg.at)
            ),
            jump: jumps.then_some((key, msg.person, msg.id)),
        })
    };
    let entries = entries(&chat.messages);
    ui.add_space(ui::SPACE_S);
    ui.label(ui::heading(t!("gallery.title")));
    ui.add_space(ui::SPACE_S);
    if entries.is_empty() && chat.at_start {
        empty(ui, t!("gallery.title"), t!("gallery.empty"));
        return;
    }
    let online = match at {
        At::Channel(index, _) => home.servers[index].status == Status::Online,
        At::Dm(peer) => (home.replicas(&peer).iter())
            .any(|(index, _)| home.servers[*index].status == Status::Online),
    };
    let hover_only = crate::emoji::hover_only(ui.ctx());
    let (mut open, mut jumped, mut more) = (None, None, false);
    ScrollArea::vertical()
        .id_salt(("gallery", key))
        .auto_shrink(false)
        .show_viewport(ui, |ui, viewport| {
            let width = ui.available_width();
            let columns = (((width + GAP) / (TILE + GAP)) as usize).max(MIN_COLUMNS);
            let tile = ((width - GAP * (columns - 1) as f32) / columns as f32).floor();
            let lines = lines(&chat.messages, &entries, columns);
            let height = |line: &Line| match line {
                Line::Month(_) => HEADER,
                Line::Tiles(_) => tile + GAP,
            };
            let spinner = if chat.at_start { 0.0 } else { 64.0 };
            let total = lines.iter().map(height).sum::<f32>() + spinner;
            let origin = ui.max_rect().min;
            ui.set_min_size(Vec2::new(width, total));
            let near = viewport.expand2(Vec2::new(0.0, tile));
            let mut y = 0.0;
            let mut playing = 0;
            for line in &lines {
                let h = height(line);
                if y + h < near.top() || y > near.bottom() {
                    y += h;
                    continue;
                }
                match line {
                    Line::Month(first) => {
                        let rect = Rect::from_min_size(
                            origin + Vec2::new(0.0, y),
                            Vec2::new(width, HEADER),
                        );
                        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                            ui.add_space(ui::SPACE_M);
                            ui.label(ui::bold(month_title(*first)));
                        });
                    }
                    Line::Tiles(range) => {
                        for (column, n) in range.clone().enumerate() {
                            let entry = &entries[n];
                            let min = origin + Vec2::new(column as f32 * (tile + GAP), y);
                            let rect = Rect::from_min_size(min, Vec2::splat(tile));
                            let Some(shot) = shot(home, entry) else {
                                continue;
                            };
                            let visible = ui.is_rect_visible(rect);
                            let play = ui.rect_contains_pointer(rect)
                                || (visible && !hover_only && playing < PLAYING);
                            let (clicked, jump) = files::gallery_tile(
                                ui,
                                &mut home.files,
                                &shot,
                                entry.kind,
                                rect,
                                play,
                            );
                            playing += usize::from(play && visible && files::animates(&shot.file));
                            if clicked {
                                open = Some(n);
                            }
                            if jump {
                                jumped = shot.jump;
                            }
                        }
                    }
                }
                y += h;
            }
            if !chat.at_start {
                let rect = Rect::from_min_size(origin + Vec2::new(0.0, y), Vec2::new(width, 64.0));
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui::SPACE_L);
                        ui.spinner();
                    });
                });
                // The end is near: load the page before, as scrolling up the chat does.
                more = rect.top() - origin.y < viewport.bottom() + viewport.height();
            }
        });
    if let Some(n) = open {
        open_entry(ui, home, &entries, n, shot);
    }
    let dm = matches!(at, At::Dm(_));
    if more && online && (!chat.loading || dm) {
        let Home {
            servers, threads, ..
        } = home;
        load_older(servers, threads, at, chat);
    }
    if let Some((_, person, id)) = jumped {
        jump(ui, key, chat, person, id);
    }
}

/// A picture opens in the viewer, which steps through the gallery's pictures; a video
/// or another file opens in its own app.
fn open_entry(
    ui: &Ui,
    home: &mut Home,
    entries: &[Entry],
    n: usize,
    shot: impl Fn(&Home, &Entry) -> Option<Shot>,
) {
    let entry = entries[n];
    if entry.kind != MediaKind::Picture {
        if let Some(shot) = shot(home, &entry) {
            let holder = (&shot.address, &shot.commands);
            home.files.open_elsewhere(ui.ctx(), holder, &shot.file);
        }
        return;
    }
    let mut at = 0;
    let mut album = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if e.kind != MediaKind::Picture {
            continue;
        }
        if let Some(shot) = shot(home, e) {
            if i == n {
                at = album.len();
            }
            album.push(shot);
        }
    }
    home.files.open_album(album, at);
}

/// Back to the chat, scrolled to the message.
fn jump(ui: &Ui, key: Id, chat: &mut Chat, person: Id, id: Id) {
    set_shown(ui.ctx(), key, false);
    chat.jump_to = Some((person, id, 4));
}
