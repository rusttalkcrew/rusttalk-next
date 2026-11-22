//! A page channel: one document of tiles that people who can write there edit in place.

use std::path::PathBuf;

use eframe::egui::{Context, ScrollArea, TextEdit, Ui};
use rusttalk_core::pages::{Layout, MAX_PAGE_TEXT, MAX_PICTURES};

use super::{Home, View, channel_icon, empty, pane_title, time, toast_line};
use crate::{
    files::{self, Ask},
    net::{Command, MAX_FILE, file_name},
    server::{Chat, Id, Page, PageState, Server, Status},
    ui,
};

pub(crate) mod editor;
mod markup;
mod pictures;
mod tiles;

use editor::Editor;
use pictures::{Art, BannerChange};
use tiles::Reader;

/// Pictures picked, dropped or pasted into the page being edited. Returns a problem
/// to show.
pub(super) fn picked(
    ctx: &Context,
    channel: Id,
    state: &mut PageState,
    paths: Vec<PathBuf>,
) -> Option<String> {
    let editor = state.editor.as_mut().filter(|_| !state.saving);
    let Some(editor) = editor else {
        // The edit ended while the picker was open.
        paths.iter().for_each(|path| files::discard(path));
        return None;
    };
    let mut cursor = editor.written().and_then(|(field, _)| {
        let id = tiles::field(channel, field);
        TextEdit::load_state(ctx, id)?
            .cursor
            .char_range()
            .map(|range| range.primary.index.0.min(range.secondary.index.0))
    });
    let mut problem = None;
    for path in paths {
        let name = file_name(&path);
        let size = std::fs::metadata(&path).ok().filter(|meta| meta.is_file());
        let refused = match size {
            None => Some(t!("files.open_failed", name = name)),
            Some(_) if !files::is_picture(&name) => Some(t!("home.picture_only").into()),
            Some(meta) if meta.len() > MAX_FILE => Some(t!("files.too_big", name = name)),
            // It would upload and then show for nobody.
            Some(_) if !files::decodes(&path) => Some(t!(
                "home.picture_unfit",
                name = name,
                max = rusttalk_core::client::PICTURE_EDGE
            )),
            Some(_) if !editor.takes_picture() => {
                Some(t!("home.picture_limit", max = MAX_PICTURES))
            }
            Some(_) => None,
        };
        if let Some(refused) = refused {
            problem = Some(refused);
            files::discard(&path);
            continue;
        }
        let Some(file) = crate::net::stand_in(&path) else {
            problem = Some(t!("files.open_failed", name = name));
            files::discard(&path);
            continue;
        };
        let id = file.file.id;
        let placed = if std::mem::take(&mut editor.for_banner) {
            editor.set_banner(file);
            true
        } else {
            let placed = editor.insert_picture(file, cursor);
            // The next one follows it.
            cursor = Some(0);
            placed
        };
        if placed {
            editor.local.push((id, path));
        } else {
            problem = Some(t!("home.tiles_full").into());
            files::discard(&path);
        }
    }
    editor.for_banner = false;
    problem
}

impl PageState {
    /// Someone saved something else since this edit began.
    fn changed_meanwhile(&self, mine: &Layout) -> bool {
        let (Some(editor), Some(page), false) = (&self.editor, &self.page, self.saving) else {
            return false;
        };
        page.version != editor.base && page.layout != *mine
    }
}

/// How much of the page's space is left, in percent rounded up; `None` when over.
fn room_left(layout: &Layout) -> Option<usize> {
    MAX_PAGE_TEXT
        .checked_sub(layout.size())
        .map(|left| (left * 100).div_ceil(MAX_PAGE_TEXT))
}

/// What the editor asked for this frame, besides changing the page.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    Banner,
    Picture,
}

enum Act {
    Edit,
    Save(i64, Box<Layout>),
    Cancel,
    Retry,
}

pub(super) fn view(ui: &mut Ui, home: &mut Home, index: usize, channel: Id, chat: &mut Chat) {
    let in_server = matches!(home.view, View::Server(_));
    let server = &home.servers[index];
    let title = server
        .channel(&channel)
        .map_or_else(String::new, |c| format!("{} {}", channel_icon(c), c.name));
    pane_title(
        ui,
        "chat-title",
        &title,
        in_server.then_some(&mut home.layout),
        false,
        Some(channel),
    );
    toast_line(ui, &mut home.toast);
    let Home {
        servers,
        files: store,
        toast,
        ..
    } = home;
    let server = &servers[index];
    let online = server.status == Status::Online;
    // The directory says it once the server has been reached.
    if online && !server.dir.pages {
        empty(ui, &title, t!("home.page_server_old"));
        return;
    }
    let can_edit = server.can_write(&channel);
    let editing = can_edit && chat.page.editor.is_some();
    let mine = (chat.page.editor.as_ref())
        .filter(|_| editing)
        .map(Editor::layout);
    let changed = (mine.as_ref()).is_some_and(|mine| chat.page.changed_meanwhile(mine));
    let mut act = None;

    ui.add_space(ui::SPACE_S);
    if can_edit && let Some(page) = &chat.page.page {
        if changed {
            ui.colored_label(ui::colors(ui).warning, t!("home.page_changed"));
            ui.add_space(ui::SPACE_XS);
        }
        ui.horizontal_wrapped(|ui| {
            if let Some(mine) = &mine {
                let room = room_left(mine);
                // An emptied page may be saved; a page never written has nothing to save.
                let written = page.version > 0 || !mine.is_empty();
                let ready = online && !chat.page.saving && room.is_some() && written;
                ui.add_enabled_ui(ready, |ui| {
                    let clicked = if changed {
                        ui::danger(ui, t!("home.page_replace_change")).clicked()
                    } else {
                        ui::primary(ui, t!("common.save")).clicked()
                    };
                    if clicked {
                        act = Some(Act::Save(page.version, Box::new(mine.clone())));
                    }
                });
                ui.add_enabled_ui(!chat.page.saving, |ui| {
                    if ui::secondary(ui, t!("common.cancel")).clicked() {
                        act = Some(Act::Cancel);
                    }
                });
                if chat.page.saving {
                    ui.spinner();
                }
                match room {
                    Some(percent) => ui.weak(t!("home.page_room", percent = percent)),
                    None => ui.colored_label(ui::colors(ui).danger_text, t!("home.page_too_long")),
                };
            } else {
                let label = match page.readable() {
                    true => t!("home.edit_page"),
                    false => t!("home.page_replace"),
                };
                if ui::secondary(ui, &format!("✏ {label}")).clicked() {
                    act = Some(Act::Edit);
                }
            }
        });
        ui.add_space(ui::SPACE_S);
    }

    let mut pick = None;
    let now = ui.input(|input| input.time);
    if editing && online && !chat.page.saving {
        let dropped: Vec<PathBuf> = ui.input(|input| {
            (input.raw.dropped_files.iter())
                .map(|file| file.path().to_owned())
                .collect()
        });
        if !dropped.is_empty()
            && let Some(problem) = picked(ui.ctx(), channel, &mut chat.page, dropped)
        {
            *toast = Some((problem, now + 6.0));
        }
        // Ctrl+V on the page, not in some dialog over it, may paste a picture.
        let here = match ui.memory(|memory| memory.focused()) {
            None => true,
            Some(focused) => {
                focused == tiles::title_field(channel)
                    || (chat.page.editor.as_mut())
                        .and_then(Editor::written)
                        .is_some_and(|(field, _)| focused == tiles::field(channel, field))
            }
        };
        if here && crate::paste::wanted(ui.ctx()) {
            let ask = Ask::PagePictures(server.address.clone(), channel);
            store.paste(ui.ctx(), ask);
        }
    }
    if let Some((what, done)) = &chat.uploading {
        ui.horizontal(|ui| {
            ui.weak(t!("home.uploading", file = what));
            ui.add(
                eframe::egui::ProgressBar::new(*done)
                    .show_percentage()
                    .desired_width(200.0),
            );
        });
    }
    let PageState {
        page,
        editor,
        failed,
        saving,
    } = &mut chat.page;
    let mut art = Art {
        files: store,
        address: &server.address,
        commands: &server.commands,
        local: (editor.as_ref()).map_or_else(Vec::new, |editor| editor.local.clone()),
    };
    if editing && let Some(editor) = editor.as_mut() {
        // The bar stays put while the page scrolls under it.
        ui.horizontal_wrapped(|ui| {
            let ready = online && !*saving;
            let size = eframe::egui::vec2(34.0, ui::CONTROL_HEIGHT);
            ui.add_enabled_ui(ready, |ui| {
                let tip = t!("home.page_add_pictures");
                if ui::icon_button(ui, "🖼", tip, ui::Look::Plain, size).clicked() {
                    pick = Some(Pick::Picture);
                }
            });
            if editor.banner.is_none() {
                ui.add_enabled_ui(ready, |ui| {
                    if ui::secondary(ui, &format!("+ {}", t!("home.banner_add"))).clicked() {
                        pick = Some(Pick::Banner);
                    }
                });
            }
            ui.add_space(ui::SPACE_M);
            let writing =
                (editor.written()).map(|(field, text)| (tiles::field(channel, field), text));
            markup::toolbar(ui, writing.filter(|_| !*saving));
        });
        ui.add_space(ui::SPACE_S);
    }
    // egui fades scrolled edges to the pane's colour, which is not the page's.    ui.spacing_mut().scroll.fade.strength = 0.0;
    ScrollArea::vertical()
        .id_salt(("page", channel))
        .auto_shrink(false)
        .show_viewport(ui, |ui, viewport| {
            ui.set_width(ui.available_width());
            tiles::surface(ui);
            let widest = tiles::MAX_PAGE;
            if editing && let Some(editor) = editor.as_mut() {
                if let Some(banner) = editor.banner.clone() {
                    let change = ui
                        .add_enabled_ui(!*saving, |ui| {
                            pictures::edit_banner(ui, &mut art, &banner, widest, channel)
                        })
                        .inner;
                    if let Some(change) = change {
                        if matches!(change, BannerChange::Replace) && online {
                            pick = Some(Pick::Banner);
                        } else {
                            editor.change_banner(change);
                        }
                    }
                }
            } else if let Some(page) = page.as_ref().filter(|page| page.readable())
                && let Some(banner) = &page.layout.banner
            {
                pictures::banner(ui, &mut art, banner, widest);
            }
            tiles::column(ui, |ui| {
                if editing && let Some(editor) = editor.as_mut() {
                    let reader = Reader {
                        dir: &server.dir,
                        channel,
                        person: server.dir.me,
                    };
                    ui.add_enabled_ui(!*saving, |ui| {
                        tiles::edit(ui, reader, &mut art, editor, viewport)
                    });
                    if !changed {
                        return;
                    }
                    ui.add_space(ui::SPACE_L);
                    ui::section(ui, t!("home.page_current"));
                    if let Some(page) = page.as_ref().filter(|page| page.readable())
                        && let Some(banner) = &page.layout.banner
                    {
                        pictures::banner(ui, &mut art, banner, widest);
                    }
                }
                match page.as_ref() {
                    Some(page) => document(ui, &mut art, server, channel, page, &title, can_edit),
                    None if *failed => {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui::SPACE_XL);
                            ui.weak(t!("home.page_failed"));
                            ui.add_space(ui::SPACE_S);
                            ui.add_enabled_ui(online, |ui| {
                                if ui::secondary(ui, t!("common.try_again")).clicked() {
                                    act = Some(Act::Retry);
                                }
                            });
                        });
                    }
                    None => {
                        ui.vertical_centered(|ui| ui.spinner());
                    }
                }
            });
        });

    if let Some(pick) = pick
        && let Some(editor) = editor.as_mut()
    {
        editor.for_banner = pick == Pick::Banner;
        let ask = Ask::PagePictures(server.address.clone(), channel);
        art.files.ask(ui.ctx(), ask);
    }
    match act {
        Some(Act::Edit) => {
            if let Some(page) = page.as_ref() {
                *editor = Some(Editor::new(page.version, &page.layout));
            }
        }
        Some(Act::Save(base, layout)) => {
            let layout = *layout;
            let mut uploads = Vec::new();
            if let Some(editor) = editor.as_mut() {
                editor.base = base;
                uploads = editor.uploads(&layout);
            }
            *saving = true;
            server.send(Command::SavePage {
                channel,
                base,
                layout,
                uploads,
            });
        }
        Some(Act::Cancel) => {
            if let Some(editor) = editor.take() {
                editor.close();
            }
        }
        Some(Act::Retry) => {
            *failed = false;
            server.send(Command::Open(channel));
        }
        None => {}
    }
}

fn document(
    ui: &mut Ui,
    art: &mut Art,
    server: &Server,
    channel: Id,
    page: &Page,
    title: &str,
    can_edit: bool,
) {
    if page.outdated {
        ui.colored_label(ui::colors(ui).warning, t!("home.page_outdated"));
        return;
    }
    if !page.readable() {
        ui.weak(t!("home.page_unreadable"));
        return;
    }
    match &page.msg {
        Some(msg) if !page.layout.is_empty() => {
            let reader = Reader {
                dir: &server.dir,
                channel,
                person: msg.person,
            };
            tiles::show(ui, reader, art, &page.layout);
            ui.add_space(ui::SPACE_S);
            let edited = t!(
                "home.page_updated",
                name = server.member_name(&msg.person),
                time = time(msg.at)
            );
            tiles::caption(ui, &edited);
        }
        _ if can_edit => nothing(ui, title, t!("home.page_empty_editor")),
        _ => nothing(ui, title, t!("home.page_empty")),
    }
}

/// `empty` for inside the page's column, which has no height of its own.
fn nothing(ui: &mut Ui, title: &str, hint: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui::SPACE_XL * 4.0);
        ui.label(ui::heading(title));
        ui.weak(hint);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editing(base: i64, version: i64, theirs: &str) -> PageState {
        PageState {
            page: Some(Page {
                version,
                layout: Layout::text(theirs),
                ..Page::default()
            }),
            editor: Some(Editor::new(base, &Layout::text("mine"))),
            ..PageState::default()
        }
    }

    #[test]
    fn warns_when_someone_saved_something_else_during_the_edit() {
        let mine = Layout::text("mine");
        assert!(!editing(2, 2, "old").changed_meanwhile(&mine));
        assert!(editing(2, 3, "theirs").changed_meanwhile(&mine));
        // The same page, like a save whose answer was lost.
        assert!(!editing(2, 3, "mine").changed_meanwhile(&mine));
        let mut saving = editing(2, 3, "theirs");
        saving.saving = true;
        assert!(
            !saving.changed_meanwhile(&mine),
            "my own save comes back first"
        );
    }

    #[test]
    fn room_left_counts_bytes() {
        let room = |text: String| room_left(&Layout::text(&text));
        assert_eq!(room(String::new()), Some(100));
        assert_eq!(room("x".repeat(MAX_PAGE_TEXT - 8)), Some(0));
        assert_eq!(room("x".repeat(MAX_PAGE_TEXT - 7)), None);
        assert_eq!(room("🦀".repeat(MAX_PAGE_TEXT / 4 + 1)), None);
    }
}
