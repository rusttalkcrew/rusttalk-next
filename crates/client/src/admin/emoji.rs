//! Server settings → Emoji and → Stickers, one page each. Pictures are picked (or
//! dropped) first, shown large with a name taken from the file and checked as it is
//! typed, then uploaded together. Packs fold open and closed and show a grid of their
//! emoji or stickers; a click opens an editor for the name, the pack and Delete. The
//! server can't swap an emoji's picture, so the editor offers no Replace.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::mpsc,
};

use eframe::egui::{
    self, Align, ComboBox, CursorIcon, Frame, Layout, Margin, Rect, RichText, Sense, Stroke, Ui,
    Vec2, WidgetInfo,
    accesskit::Role,
    text::{LayoutJob, TextFormat, TextWrapping},
};

use crate::{
    net::Admin,
    server::{CustomEmoji, Directory, Id, Picture},
    ui::{self, Item},
};

/// A picked file larger than this is refused without being read.
const MAX_FILE: u64 = 8 * 1024 * 1024;
/// Pictures in the packs' grids and in the upload list.
const EMOJI_TILE: f32 = 48.0;
const STICKER_TILE: f32 = 120.0;
/// Emoji tiles are wider than their picture, so names fit under them.
const MIN_TILE_WIDTH: f32 = 84.0;
/// The editor's preview.
const EMOJI_LARGE: f32 = 128.0;
const STICKER_LARGE: f32 = 240.0;
/// A pack name is short, like the other names in the app.
const MAX_PACK_NAME: usize = 32;

/// A picked picture waiting for Upload.
struct Staged {
    slot: u64,
    path: PathBuf,
    /// The file's own name, to tell the pictures apart.
    file: String,
    name: String,
    prep: Prep,
}

/// A picked picture made ready on a background thread: scaled and re-encoded the way
/// it will be uploaded, or why it can't be.
enum Prep {
    Loading,
    Ready { bytes: Vec<u8>, picture: Picture },
    Bad(&'static str),
}

#[derive(Default)]
struct Upload {
    items: Vec<Staged>,
    /// The pack they go to; the first when `None`.
    pack: Option<Id>,
    /// Sent and not answered yet.
    sending: bool,
}

/// The emoji or sticker open in the editor, as edited so far.
struct Editor {
    id: Id,
    name: String,
    pack: Id,
    /// Delete was clicked: the editor asks to confirm.
    deleting: bool,
    /// A change was sent; its error shows here.
    sent: bool,
}

/// "New pack" (`id` is `None`) or "Rename pack".
struct PackName {
    id: Option<Id>,
    name: String,
    sent: bool,
}

/// A picked picture made ready: its slot in the upload list, and how it went.
type Prepared = (u64, Prep);

/// The state of the Emoji and Stickers pages.
pub struct State {
    /// Emoji first, then stickers.
    uploads: [Upload; 2],
    editor: Option<Editor>,
    pack_name: Option<PackName>,
    /// The pack waiting for its delete to be confirmed, and whether it was sent.
    pack_deleting: Option<(Id, bool)>,
    /// Packs folded shut.
    folded: HashSet<Id>,
    next_slot: u64,
    loaded: (mpsc::Sender<Prepared>, mpsc::Receiver<Prepared>),
    /// Files are being dragged over the window.
    pub hovering: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            uploads: Default::default(),
            editor: None,
            pack_name: None,
            pack_deleting: None,
            folded: HashSet::new(),
            next_slot: 0,
            loaded: mpsc::channel(),
            hovering: false,
        }
    }
}

/// What the page asks for this frame.
#[derive(Default)]
pub struct Asked {
    /// Open the file dialog for pictures: stickers when `true`.
    pub pick: Option<bool>,
    /// Pick a .zip to import as a pack.
    pub zip: bool,
    /// Crop this picked picture again: its slot, file and whether it is a sticker.
    pub crop: Option<(u64, PathBuf, bool)>,
    pub request: Option<Admin>,
}

impl State {
    /// A change went through.
    pub fn done(&mut self) {
        self.editor = None;
        self.pack_name = None;
        self.pack_deleting = None;
        self.failed();
    }

    /// A change was refused, or only part of an upload went through: what is left
    /// stays, to fix and send again.
    pub fn failed(&mut self) {
        for upload in &mut self.uploads {
            upload.sending = false;
        }
    }

    /// Picked pictures wait for Upload: closing would throw them away.
    pub fn unsaved(&self) -> bool {
        self.uploads.iter().any(|upload| !upload.items.is_empty())
    }

    /// This name was uploaded: it leaves the list. Others stay until theirs is.
    pub fn added(&mut self, name: &str) {
        for upload in self.uploads.iter_mut().filter(|upload| upload.sending) {
            if let Some(at) = upload.items.iter().position(|item| item.name == name) {
                upload.items.remove(at);
                return;
            }
        }
    }

    /// Add picked or dropped files to the emoji (or `sticker`) upload list. They are
    /// read and fitted on a background thread; `poll` picks the results up.
    pub fn stage(&mut self, sticker: bool, paths: Vec<PathBuf>, ctx: &egui::Context) {
        let upload = &mut self.uploads[usize::from(sticker)];
        if upload.sending || paths.is_empty() {
            return;
        }
        let mut work = Vec::new();
        for path in paths {
            let slot = self.next_slot;
            self.next_slot += 1;
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            upload.items.push(Staged {
                slot,
                file: (path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                name: crate::emoji_zip::sanitize(stem.as_deref().unwrap_or_default()),
                prep: Prep::Loading,
                path: path.clone(),
            });
            work.push((slot, path));
        }
        let (sender, ctx) = (self.loaded.0.clone(), ctx.clone());
        let spawned = std::thread::Builder::new()
            .name("emoji pictures".into())
            .spawn(move || {
                for (slot, path) in work {
                    let _ = sender.send((slot, prepare(&path, sticker)));
                    ctx.request_repaint();
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start reading pictures");
            for item in &mut upload.items {
                if matches!(item.prep, Prep::Loading) {
                    item.prep = Prep::Bad(t!("admin.upload_unreadable"));
                }
            }
        }
    }

    /// A picked picture cropped again by hand.
    pub fn cropped(&mut self, slot: u64, bytes: Vec<u8>) {
        let prep = ready(bytes);
        if let Some(item) = (self.uploads.iter_mut())
            .flat_map(|upload| &mut upload.items)
            .find(|item| item.slot == slot)
        {
            item.prep = prep;
        }
    }

    /// Pictures made ready since the last frame.
    fn poll(&mut self) {
        while let Ok((slot, prep)) = self.loaded.1.try_recv() {
            if let Some(item) = (self.uploads.iter_mut())
                .flat_map(|upload| &mut upload.items)
                .find(|item| item.slot == slot)
            {
                item.prep = prep;
            }
        }
    }
}

/// Read, check and fit one picked file the way the upload will send it.
fn prepare(path: &Path, sticker: bool) -> Prep {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_FILE => return Prep::Bad(t!("admin.upload_too_big")),
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%error, "reading a picked picture failed");
            return Prep::Bad(t!("admin.upload_unreadable"));
        }
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "reading a picked picture failed");
            return Prep::Bad(t!("admin.upload_unreadable"));
        }
    };
    let known = image::guess_format(&bytes).is_ok_and(|format| {
        use image::ImageFormat::{Gif, Jpeg, Png, WebP};
        matches!(format, Png | Jpeg | Gif | WebP)
    });
    if !known {
        return Prep::Bad(t!("admin.upload_wrong_type"));
    }
    match crate::crop::fit(&bytes, sticker) {
        Ok(fitted) => ready(fitted),
        Err(error) => {
            tracing::info!(error = %format_args!("{error:#}"), "picked picture refused");
            Prep::Bad(t!("admin.upload_unreadable"))
        }
    }
}

fn ready(bytes: Vec<u8>) -> Prep {
    match crate::net::picture(&bytes) {
        Some(picture) => Prep::Ready { bytes, picture },
        None => Prep::Bad(t!("admin.upload_unreadable")),
    }
}

/// How many emoji (or stickers) the server has, and may have.
fn usage(dir: &Directory, sticker: bool) -> (usize, usize) {
    let count = dir.emoji.iter().filter(|e| e.sticker == sticker).count();
    let max = if sticker {
        dir.emoji_max.1
    } else {
        dir.emoji_max.0
    };
    (count, max)
}

/// What is wrong with each picked picture, if anything.
fn problems(dir: &Directory, sticker: bool, items: &[Staged]) -> Vec<Option<&'static str>> {
    (items.iter().enumerate())
        .map(|(at, item)| match &item.prep {
            Prep::Bad(text) => Some(*text),
            _ if !crate::emoji::name_ok(&item.name) => Some(t!("admin.name_invalid")),
            _ if (dir.emoji.iter()).any(|e| e.sticker == sticker && e.name == item.name)
                || items[..at].iter().any(|other| other.name == item.name) =>
            {
                Some(t!("admin.name_taken"))
            }
            _ => None,
        })
        .collect()
}

/// The pack `wanted`, while it still exists, or else the first.
fn pack_or_first(dir: &Directory, wanted: Option<Id>) -> Option<Id> {
    wanted
        .filter(|id| dir.packs.iter().any(|pack| pack.id == *id))
        .or_else(|| dir.packs.first().map(|pack| pack.id))
}

/// A pack picker labelled "Pack".
fn pack_choice(ui: &mut Ui, dir: &Directory, salt: &str, pack: &mut Option<Id>) {
    ui::text_row(ui, false, |ui| {
        let label = ui.label(ui::bold(t!("admin.pack")));
        let shown = (pack.and_then(|id| dir.packs.iter().find(|p| p.id == id)))
            .map(|pack| pack.title())
            .unwrap_or_default();
        let width = ui.available_width().min(ui.spacing().combo_width * 1.5);
        let combo = ComboBox::from_id_salt(salt)
            .width(width)
            .truncate()
            .selected_text(shown)
            .show_ui(ui, |ui| {
                for item in &dir.packs {
                    if ui
                        .selectable_label(*pack == Some(item.id), item.title())
                        .clicked()
                    {
                        *pack = Some(item.id);
                    }
                }
            });
        combo.response.labelled_by(label.id);
    });
}

/// The Emoji page (or with `sticker`, the Stickers page). `picker`: this device has a
/// file dialog.
pub fn page(ui: &mut Ui, dir: &Directory, state: &mut State, sticker: bool, picker: bool) -> Asked {
    state.poll();
    let mut asked = Asked::default();
    ui::page_section(ui, t!("admin.upload"));
    upload(ui, dir, state, sticker, picker, &mut asked);
    ui::page_section(ui, t!("admin.packs"));
    let (count, max) = usage(dir, sticker);
    let room = dir.packs.len() < dir.packs_max;
    ui::trailing_row(
        ui,
        |ui| {
            let zip = ui
                .add_enabled_ui(room && picker, |ui| {
                    ui::secondary(ui, t!("admin.import_zip"))
                })
                .inner
                .on_hover_text(t!("admin.import_help"));
            asked.zip = zip.clicked();
            let add = ui.add_enabled_ui(room, |ui| ui::secondary(ui, t!("admin.pack_new")));
            if add.inner.clicked() {
                state.pack_name = Some(PackName {
                    id: None,
                    name: String::new(),
                    sent: false,
                });
            }
        },
        |ui| {
            let used = if sticker {
                t!("admin.stickers_used", count = count, max = max)
            } else {
                t!("admin.emoji_used", count = count, max = max)
            };
            ui.label(used);
            if !room {
                ui.label(ui::caption(t!("admin.pack_limit")));
            }
        },
    );
    for (at, pack) in dir.packs.iter().enumerate() {
        ui.push_id(pack.id, |ui| {
            pack_block(ui, dir, state, sticker, at, &mut asked)
        });
    }
    asked
}

/// Where pictures are picked or dropped, and the list of picked ones.
fn upload(
    ui: &mut Ui,
    dir: &Directory,
    state: &mut State,
    sticker: bool,
    picker: bool,
    asked: &mut Asked,
) {
    let (count, max) = usage(dir, sticker);
    let full = count >= max;
    let sending = state.uploads[usize::from(sticker)].sending;
    let c = ui::colors(ui);
    let accent = ui::accent(ui).base;
    let hover = state.hovering && picker && !full && !sending;
    let (stroke, fill) = if hover {
        (Stroke::new(2.0, accent), accent.gamma_multiply(0.12))
    } else {
        (Stroke::new(1.0, c.border), c.field)
    };
    Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(ui::RADIUS)
        .inner_margin(Margin::same(ui::SPACE_L as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.spacing_mut().item_spacing.y = ui::SPACE_S;
                // Phones have no files to drag in.
                if picker && !ui::TOUCH {
                    ui.label(ui::bold(if sticker {
                        t!("admin.drop_stickers")
                    } else {
                        t!("admin.drop_emoji")
                    }));
                }
                let choose = ui
                    .add_enabled_ui(picker && !full && !sending, |ui| {
                        ui::primary(ui, t!("admin.choose_pictures"))
                    })
                    .inner;
                if choose.clicked() {
                    asked.pick = Some(sticker);
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(if sticker {
                            t!("admin.sticker_help")
                        } else {
                            t!("admin.emoji_help")
                        })
                        .color(c.muted),
                    )
                    .wrap(),
                );
            });
        });
    if full {
        ui::error_text(
            ui,
            Some(if sticker {
                t!("admin.sticker_limit")
            } else {
                t!("admin.emoji_limit")
            }),
        );
    }
    if !state.uploads[usize::from(sticker)].items.is_empty() {
        ui.add_space(ui::SPACE_S);
        picked(ui, dir, state, sticker, picker, asked);
    }
}

/// The picked pictures: big previews, names to edit, what is wrong, the pack and Upload.
fn picked(
    ui: &mut Ui,
    dir: &Directory,
    state: &mut State,
    sticker: bool,
    picker: bool,
    asked: &mut Asked,
) {
    let (count, max) = usage(dir, sticker);
    let upload = &mut state.uploads[usize::from(sticker)];
    let problems = problems(dir, sticker, &upload.items);
    let size = if sticker { STICKER_TILE } else { 64.0 };
    let c = ui::colors(ui);
    let mut remove = None;
    ui.add_enabled_ui(!upload.sending, |ui| {
        for (at, item) in upload.items.iter_mut().enumerate() {
            ui.push_id(item.slot, |ui| {
                ui::trailing_row(
                    ui,
                    |ui| {
                        let tip = t!("admin.upload_remove", file = item.file);
                        if ui::small_close(ui, &tip).clicked() {
                            remove = Some(at);
                        }
                        if picker
                            && matches!(item.prep, Prep::Ready { .. })
                            && ui::icon_button(
                                ui,
                                "✂",
                                &t!("admin.upload_crop", file = item.file),
                                ui::Look::Ghost,
                                Vec2::splat(ui::ICON_BUTTON),
                            )
                            .clicked()
                        {
                            asked.crop = Some((item.slot, item.path.clone(), sticker));
                        }
                    },
                    |ui| {
                        ui.horizontal_top(|ui| {
                            let (rect, _) =
                                ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
                            match &item.prep {
                                Prep::Ready { picture, .. } => {
                                    crate::home::paint_hovered(
                                        ui,
                                        rect,
                                        picture,
                                        ui::RADIUS_S.into(),
                                    );
                                }
                                Prep::Loading => {
                                    ui.painter().rect_filled(rect, ui::RADIUS_S, c.hover);
                                    ui.put(rect, egui::Spinner::new());
                                }
                                Prep::Bad(_) => {
                                    ui.painter().rect_filled(rect, ui::RADIUS_S, c.hover);
                                    ui.painter().text(
                                        rect.center(),
                                        egui::Align2::CENTER_CENTER,
                                        "⚠",
                                        egui::FontId::proportional(ui::TEXT_ICON_LARGE),
                                        c.danger_text,
                                    );
                                }
                            }
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = ui::SPACE_XS;
                                ui.add(egui::Label::new(ui::caption(&item.file)).truncate());
                                let field = ui.add(
                                    ui::field(&mut item.name)
                                        .char_limit(32)
                                        .desired_width(ui.available_width().min(260.0)),
                                );
                                field.widget_info(|| {
                                    WidgetInfo::labeled(
                                        Role::TextInput,
                                        true,
                                        t!("admin.upload_name", file = item.file),
                                    )
                                });
                                if field.changed() {
                                    item.name = crate::emoji::clean_name(&item.name);
                                }
                                if let Some(problem) = problems[at] {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(problem).color(c.danger_text),
                                        )
                                        .wrap(),
                                    );
                                } else if matches!(item.prep, Prep::Loading) {
                                    ui.label(ui::caption(t!("admin.upload_preparing")));
                                } else {
                                    ui.label(ui::caption(format!(":{}:", item.name)));
                                }
                            });
                        });
                    },
                );
            });
            ui.add_space(ui::SPACE_S);
        }
    });
    if let Some(at) = remove {
        upload.items.remove(at);
    }
    if upload.items.is_empty() {
        return;
    }
    if dir.packs.len() > 1 {
        ui.add_enabled_ui(!upload.sending, |ui| {
            upload.pack = pack_or_first(dir, upload.pack);
            pack_choice(ui, dir, "upload-pack", &mut upload.pack);
        });
    }
    let room = max.saturating_sub(count);
    let wrong = problems.iter().flatten().count();
    let loading = (upload.items.iter()).any(|item| matches!(item.prep, Prep::Loading));
    let warning = if upload.items.len() > room {
        Some(t!("admin.upload_room", count = room))
    } else if wrong > 0 {
        Some(t!("admin.upload_fix", count = wrong))
    } else {
        None
    };
    ui::error_text(ui, warning.as_deref());
    if upload.sending {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(t!("admin.uploading"));
        });
    }
    let ready = warning.is_none() && !loading && !upload.sending;
    let total = upload.items.len();
    let mut go = false;
    let mut clear = false;
    ui::footer(ui, |ui| {
        let text = if sticker {
            t!("admin.upload_stickers", count = total)
        } else {
            t!("admin.upload_emoji", count = total)
        };
        go = ui
            .add_enabled_ui(ready, |ui| ui::primary(ui, &text))
            .inner
            .clicked();
        clear = ui
            .add_enabled_ui(!upload.sending, |ui| {
                ui::secondary(ui, t!("admin.upload_clear"))
            })
            .inner
            .clicked();
    });
    if clear {
        upload.items.clear();
    }
    if go {
        let pack = pack_or_first(dir, upload.pack);
        let mut adds: Vec<Admin> = (upload.items.iter())
            .filter_map(|item| match &item.prep {
                Prep::Ready { bytes, .. } => Some(Admin::AddEmoji {
                    name: item.name.clone(),
                    sticker,
                    bytes: bytes.clone(),
                    pack,
                }),
                _ => None,
            })
            .collect();
        upload.sending = true;
        asked.request = Some(if adds.len() == 1 {
            adds.remove(0)
        } else {
            Admin::Batch(adds)
        });
    }
}

/// One pack: a header that folds it, with its "…" menu, and a grid of its emoji or
/// stickers.
fn pack_block(
    ui: &mut Ui,
    dir: &Directory,
    state: &mut State,
    sticker: bool,
    at: usize,
    asked: &mut Asked,
) {
    let pack = &dir.packs[at];
    let title = pack.title();
    let items: Vec<&CustomEmoji> = (dir.emoji.iter())
        .filter(|e| e.pack == pack.id && e.sticker == sticker)
        .collect();
    let open = !state.folded.contains(&pack.id);
    ui.add_space(ui::SPACE_S);
    ui::trailing_row(
        ui,
        |ui| {
            let more = ui::icon_button(
                ui,
                "…",
                &t!("admin.pack_actions", name = title),
                ui::Look::Ghost,
                Vec2::splat(ui::ICON_BUTTON),
            );
            let last = dir.packs.len();
            ui::menu(&more, |m| {
                if m.item("✏", t!("admin.pack_rename")).clicked() {
                    state.pack_name = Some(PackName {
                        id: Some(pack.id),
                        name: pack.name.clone().unwrap_or_default(),
                        sent: false,
                    });
                }
                for (icon, label, to) in [
                    ("⬆", t!("admin.move_up"), at.checked_sub(1)),
                    (
                        "⬇",
                        t!("admin.move_down"),
                        Some(at + 1).filter(|to| *to < last),
                    ),
                ] {
                    if m.add(Item::new(icon, label).enabled(to.is_some()))
                        .clicked()
                        && let Some(to) = to
                    {
                        let mut ids: Vec<Id> = dir.packs.iter().map(|p| p.id).collect();
                        ids.swap(at, to);
                        asked.request = Some(Admin::OrderPacks(ids));
                    }
                }
                m.separator();
                let delete = Item::new("🗑", t!("admin.pack_delete"))
                    .danger()
                    .enabled(last > 1);
                if m.add(delete).clicked() {
                    state.pack_deleting = Some((pack.id, false));
                }
            });
        },
        |ui| {
            let label = t!("admin.pack_fold", name = title, count = items.len());
            let row = ui::row_button(ui, ui::ROW_HEIGHT, &label, |ui| {
                let openness = ui.ctx().animate_bool(ui.make_persistent_id("open"), open);
                let (_, response) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                egui::collapsing_header::paint_default_icon(ui, openness, &response);
                ui.label(ui::bold(&title));
                ui.label(ui::caption(items.len().to_string()));
            });
            if row.clicked() && !state.folded.remove(&pack.id) {
                state.folded.insert(pack.id);
            }
        },
    );
    if !open {
        return;
    }
    if items.is_empty() {
        ui.indent("empty", |ui| {
            ui.weak(if sticker {
                t!("admin.pack_no_stickers")
            } else {
                t!("admin.pack_no_emoji")
            });
        });
        return;
    }
    let size = if sticker { STICKER_TILE } else { EMOJI_TILE };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(ui::SPACE_XS);
        for item in items {
            if tile(ui, item, size).clicked() {
                state.editor = Some(Editor {
                    id: item.id,
                    name: item.name.clone(),
                    pack: item.pack,
                    deleting: false,
                    sent: false,
                });
            }
        }
    });
}

/// One emoji or sticker in a pack's grid: its picture (playing while hovered) and
/// its name under it. Clicking opens the editor.
fn tile(ui: &mut Ui, item: &CustomEmoji, size: f32) -> egui::Response {
    let pad = ui::SPACE_S;
    let text = ui::TEXT_CAPTION + 4.0;
    let width = (size + 2.0 * pad).max(MIN_TILE_WIDTH);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(width, pad + size + ui::SPACE_XS + text + pad),
        Sense::click(),
    );
    let c = ui::colors(ui);
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered() || response.has_focus(),
        ui::HOVER_SECONDS,
    );
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        ui::RADIUS,
        egui::Color32::TRANSPARENT.lerp_to_gamma(c.hover, hover),
    );
    let picture = Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + pad + size / 2.0),
        Vec2::splat(size),
    );
    match &item.picture {
        Some(shown) => crate::home::paint_playing(ui, picture, shown, 0.0, response.hovered()),
        None => {
            painter.rect_filled(picture, ui::RADIUS_S, c.hover);
        }
    }
    let mut job = LayoutJob::single_section(
        format!(":{}:", item.name),
        TextFormat::simple(egui::FontId::proportional(ui::TEXT_CAPTION), c.text),
    );
    job.wrap = TextWrapping::truncate_at_width(width - 2.0 * ui::SPACE_XS);
    job.halign = Align::Center;
    let galley = painter.layout_job(job);
    painter.galley(
        egui::pos2(rect.center().x, picture.bottom() + ui::SPACE_XS),
        galley,
        c.text,
    );
    ui::focus_ring(ui, &response, ui::RADIUS);
    let label = t!("admin.edit_item", name = format!(":{}:", item.name));
    response.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), &label));
    response
        .on_hover_text(format!(":{}:", item.name))
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// The editor and the pack questions over the settings window. `error` is the last
/// change's error, shown in the dialog that sent it.
pub fn dialogs(
    ctx: &egui::Context,
    dir: &Directory,
    state: &mut State,
    sticker: bool,
    error: Option<&str>,
) -> Option<Admin> {
    editor(ctx, dir, state, sticker, error)
        .or_else(|| pack_name(ctx, dir, state, error))
        .or_else(|| pack_delete(ctx, dir, state, error))
}

fn editor(
    ctx: &egui::Context,
    dir: &Directory,
    state: &mut State,
    sticker: bool,
    error: Option<&str>,
) -> Option<Admin> {
    let editor = state.editor.as_mut()?;
    let Some(item) = dir.emoji.iter().find(|e| e.id == editor.id) else {
        // Deleted, from here or elsewhere.
        state.editor = None;
        return None;
    };
    let error = error.filter(|_| editor.sent);
    let shown = format!(":{}:", item.name);
    let title = match (editor.deleting, sticker) {
        (true, false) => t!("admin.delete_emoji_title"),
        (true, true) => t!("admin.delete_sticker_title"),
        (false, false) => t!("admin.edit_emoji"),
        (false, true) => t!("admin.edit_sticker"),
    };
    let mut request = None;
    let mut close = false;
    let ((), dismissed) = ui::dialog(ctx, "emoji-editor", title, |ui| {
        if editor.deleting {
            ui.label(t!("admin.emoji_delete_help", name = item.name));
            ui::error_text(ui, error);
            ui::footer(ui, |ui| {
                if ui::danger(ui, t!("common.delete")).clicked() {
                    request = Some(Admin::DeleteEmoji(item.id));
                }
                if ui::secondary(ui, t!("common.cancel")).clicked() {
                    editor.deleting = false;
                }
            });
            return;
        }
        ui.vertical_centered(|ui| {
            let large = if sticker { STICKER_LARGE } else { EMOJI_LARGE };
            let size = large.min(ui.available_width());
            match &item.picture {
                Some(picture) => {
                    crate::home::emoji_image(ui, picture, size, &shown);
                }
                None => {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
                    ui.painter()
                        .rect_filled(rect, ui::RADIUS, ui::colors(ui).hover);
                }
            }
        });
        ui.add_space(ui::SPACE_S);
        let label = ui.label(ui::bold(t!("common.name")));
        let field = ui
            .add(
                ui::field(&mut editor.name)
                    .char_limit(32)
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(label.id);
        if field.changed() {
            editor.name = crate::emoji::clean_name(&editor.name);
        }
        let taken = (dir.emoji.iter())
            .any(|e| e.sticker == item.sticker && e.name == editor.name && e.id != item.id);
        let problem = if !crate::emoji::name_ok(&editor.name) {
            Some(t!("admin.name_invalid"))
        } else if taken {
            Some(t!("admin.name_taken"))
        } else {
            None
        };
        ui::error_text(ui, problem);
        let code = format!(":{}:", editor.name);
        ui.label(ui::caption(t!("admin.name_help", code = code)));
        if dir.packs.len() > 1 {
            ui.add_space(ui::SPACE_S);
            let mut pack = Some(editor.pack);
            pack_choice(ui, dir, "editor-pack", &mut pack);
            editor.pack = pack.unwrap_or(editor.pack);
        }
        ui.add_space(ui::SPACE_S);
        ui.label(ui::caption(t!("admin.replace_help")));
        ui::error_text(ui, error);
        let renamed = editor.name != item.name;
        let moved = editor.pack != item.pack;
        let ready = problem.is_none() && (renamed || moved);
        let mut save = ready && crate::app::submitted(ui, &field);
        ui::footer(ui, |ui| {
            save |= ui
                .add_enabled_ui(ready, |ui| ui::primary(ui, t!("common.save")))
                .inner
                .clicked();
            close = ui::secondary(ui, t!("common.cancel")).clicked();
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                if ui::danger(ui, t!("common.delete")).clicked() {
                    editor.deleting = true;
                }
            });
        });
        if save {
            let mut changes = Vec::new();
            if renamed {
                changes.push(Admin::RenameEmoji(item.id, editor.name.clone()));
            }
            if moved {
                changes.push(Admin::MoveEmoji(item.id, editor.pack));
            }
            request = Some(if changes.len() == 1 {
                changes.remove(0)
            } else {
                Admin::Batch(changes)
            });
        }
    });
    if request.is_some() {
        editor.sent = true;
    }
    if close || dismissed {
        state.editor = None;
    }
    request
}

fn pack_name(
    ctx: &egui::Context,
    dir: &Directory,
    state: &mut State,
    error: Option<&str>,
) -> Option<Admin> {
    let asked = state.pack_name.as_mut()?;
    let error = error.filter(|_| asked.sent);
    let title = if asked.id.is_some() {
        t!("admin.pack_rename")
    } else {
        t!("admin.pack_new")
    };
    let mut request = None;
    let mut close = false;
    let ((), dismissed) = ui::dialog(ctx, "pack-name", title, |ui| {
        let label = ui.label(ui::bold(t!("admin.pack_name")));
        let field = ui
            .add(
                ui::field(&mut asked.name)
                    .char_limit(MAX_PACK_NAME)
                    .hint_text(t!("admin.pack_name_hint"))
                    .desired_width(f32::INFINITY),
            )
            .labelled_by(label.id);
        ui::focus_when_none(ui, &field);
        ui::error_text(ui, error);
        let name = asked.name.trim();
        let ready = !name.is_empty() && (asked.id.is_some() || dir.packs.len() < dir.packs_max);
        let mut save = ready && crate::app::submitted(ui, &field);
        ui::footer(ui, |ui| {
            let text = if asked.id.is_some() {
                t!("common.save")
            } else {
                t!("admin.pack_create")
            };
            save |= ui
                .add_enabled_ui(ready, |ui| ui::primary(ui, text))
                .inner
                .clicked();
            close = ui::secondary(ui, t!("common.cancel")).clicked();
        });
        if save {
            request = Some(match asked.id {
                Some(id) => Admin::RenamePack(id, name.to_owned()),
                None => Admin::AddPack(name.to_owned()),
            });
        }
    });
    if request.is_some() {
        asked.sent = true;
    }
    if close || dismissed {
        state.pack_name = None;
    }
    request
}

fn pack_delete(
    ctx: &egui::Context,
    dir: &Directory,
    state: &mut State,
    error: Option<&str>,
) -> Option<Admin> {
    let (id, sent) = state.pack_deleting.as_mut()?;
    let Some(pack) = dir.packs.iter().find(|pack| pack.id == *id) else {
        state.pack_deleting = None;
        return None;
    };
    let error = error.filter(|_| *sent);
    let count = dir.emoji.iter().filter(|e| e.pack == pack.id).count();
    let mut request = None;
    let mut close = false;
    let title = t!("admin.pack_delete_title", name = pack.title());
    let ((), dismissed) = ui::dialog(ctx, "pack-delete", &title, |ui| {
        ui.label(t!("admin.pack_delete_what", count = count));
        ui::error_text(ui, error);
        ui::footer(ui, |ui| {
            if ui::danger(ui, t!("admin.pack_delete")).clicked() {
                request = Some(Admin::DeletePack(pack.id));
            }
            close = ui::secondary(ui, t!("common.cancel")).clicked();
        });
    });
    if request.is_some() {
        *sent = true;
    }
    if close || dismissed {
        state.pack_deleting = None;
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::EmojiPack;

    fn staged(name: &str, prep: Prep) -> Staged {
        Staged {
            slot: 0,
            path: PathBuf::new(),
            file: format!("{name}.png"),
            name: name.into(),
            prep,
        }
    }

    #[test]
    fn picked_pictures_are_checked_one_by_one() {
        let dir = Directory {
            emoji: vec![CustomEmoji {
                id: [1; 32],
                name: "cat".into(),
                sticker: false,
                pack: [0; 32],
                picture: None,
            }],
            packs: vec![EmojiPack {
                id: [0; 32],
                name: None,
            }],
            ..Directory::default()
        };
        let items = [
            staged("cat", Prep::Loading),
            staged("dog", Prep::Loading),
            staged("dog", Prep::Loading),
            staged("x", Prep::Loading),
            staged("big", Prep::Bad("too big")),
        ];
        assert_eq!(
            problems(&dir, false, &items),
            [
                Some(t!("admin.name_taken")),
                None,
                Some(t!("admin.name_taken")),
                Some(t!("admin.name_invalid")),
                Some("too big"),
            ]
        );
        // A sticker may share an emoji's name.
        assert_eq!(problems(&dir, true, &items[..1]), [None]);
    }
}
