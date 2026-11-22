//! "Import pack from .zip" in Server settings → Emoji & stickers: the archive is read on
//! its own thread, a preview lists every picture to rename, switch between emoji and
//! sticker or skip, and then the app uploads them into a new pack.

use std::{
    path::{Path, PathBuf},
    sync::mpsc,
};

use eframe::egui::{
    self, ColorImage, Context, Role, ScrollArea, TextureHandle, TextureOptions, Ui, Vec2,
};

use crate::{
    emoji_zip::{self, Problem},
    net::{Admin, ImportItem},
    server::Directory,
    ui,
};

/// Pictures up to this size on a side start out as emoji, bigger ones as stickers.
const EMOJI_SIDE: u32 = 160;
/// A pack name is short, like the other names in the app.
const MAX_PACK_NAME: usize = 32;
/// Rows beyond this are not drawn each frame; the list scrolls.
const LIST_HEIGHT: f32 = 300.0;
const THUMB: f32 = 32.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Emoji,
    Sticker,
    Skip,
}

impl Kind {
    /// The next choice when the button is clicked.
    fn next(self) -> Self {
        match self {
            Kind::Emoji => Kind::Sticker,
            Kind::Sticker => Kind::Skip,
            Kind::Skip => Kind::Emoji,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Emoji => t!("admin.emoji"),
            Kind::Sticker => t!("admin.sticker"),
            Kind::Skip => t!("admin.import_skip"),
        }
    }
}

struct Row {
    name: String,
    kind: Kind,
    bytes: Vec<u8>,
    thumb: Option<ColorImage>,
    texture: Option<TextureHandle>,
}

/// What a read gave: the pictures with their previews, or why not.
type Loaded = Result<(Vec<Row>, String), &'static str>;

enum Stage {
    Reading(mpsc::Receiver<Loaded>),
    Failed(&'static str),
    Ready,
    /// Pictures added so far, and in all.
    Uploading(usize, usize),
}

pub struct Import {
    stage: Stage,
    rows: Vec<Row>,
    name: String,
}

/// What the dialog asks for this frame.
pub enum Outcome {
    Open,
    Close,
    Start(Admin),
}

impl Import {
    /// Read `path` on its own thread; the most pictures allowed is the larger of the
    /// server's two limits (each kind is checked again before uploading).
    pub fn start(path: &Path, max: usize, ctx: &Context) -> Self {
        let (sender, receiver) = mpsc::channel();
        let (path, ctx) = (path.to_owned(), ctx.clone());
        std::thread::spawn(move || {
            let _ = sender.send(load(&path, max));
            ctx.request_repaint();
        });
        Self {
            stage: Stage::Reading(receiver),
            rows: Vec::new(),
            name: String::new(),
        }
    }

    /// An import's progress from the server loop.
    pub fn progress(&mut self, done: usize, total: usize) {
        self.stage = Stage::Uploading(done, total);
    }

    /// Back to the preview after a failed upload, so nothing typed is lost.
    pub fn failed(&mut self) {
        if matches!(self.stage, Stage::Uploading(..)) {
            self.stage = Stage::Ready;
        }
    }

    pub fn show(&mut self, ctx: &Context, dir: &Directory) -> Outcome {
        if let Stage::Reading(receiver) = &self.stage {
            match receiver.try_recv() {
                Ok(Ok((rows, name))) => {
                    self.rows = rows;
                    self.name = name;
                    self.stage = Stage::Ready;
                }
                Ok(Err(problem)) => self.stage = Stage::Failed(problem),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.stage = Stage::Failed(t!("admin.import_unreadable"));
                }
            }
        }
        let mut outcome = Outcome::Open;
        let (_, dismissed) = ui::dialog(ctx, "import-pack", t!("admin.import_title"), |ui| {
            outcome = self.body(ui, dir);
        });
        // A running import can't be dismissed; it ends with the server's answer.
        if dismissed && !matches!(self.stage, Stage::Uploading(..)) {
            return Outcome::Close;
        }
        outcome
    }

    fn body(&mut self, ui: &mut Ui, dir: &Directory) -> Outcome {
        match &self.stage {
            Stage::Reading(_) => {
                ui.add_space(ui::SPACE_L);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(t!("admin.import_reading"));
                });
                Outcome::Open
            }
            Stage::Failed(problem) => {
                ui::error_text(ui, Some(problem));
                let mut out = Outcome::Open;
                ui::footer(ui, |ui| {
                    if ui::secondary(ui, t!("common.close")).clicked() {
                        out = Outcome::Close;
                    }
                });
                out
            }
            Stage::Uploading(done, total) => {
                let fraction = *done as f32 / (*total).max(1) as f32;
                ui.add_space(ui::SPACE_S);
                ui.label(t!("admin.import_uploading", done = done, total = total));
                ui.add(egui::ProgressBar::new(fraction));
                Outcome::Open
            }
            Stage::Ready => self.preview(ui, dir),
        }
    }

    fn preview(&mut self, ui: &mut Ui, dir: &Directory) -> Outcome {
        let label = ui.label(ui::bold(t!("admin.pack_name")));
        ui.add(
            ui::field(&mut self.name)
                .char_limit(MAX_PACK_NAME)
                .hint_text(t!("admin.pack_name_hint"))
                .desired_width(f32::INFINITY),
        )
        .labelled_by(label.id);
        ui.add_space(ui::SPACE_XS);
        ui.horizontal(|ui| {
            for (kind, text) in [
                (Kind::Emoji, t!("admin.import_all_emoji")),
                (Kind::Sticker, t!("admin.import_all_stickers")),
            ] {
                if ui::secondary(ui, text).clicked() {
                    self.rows.iter_mut().for_each(|row| row.kind = kind);
                }
            }
        });
        let problems = check(&self.rows, dir);
        let taken: Vec<bool> = self
            .rows
            .iter()
            .map(|row| row.is_taken(&problems))
            .collect();
        ScrollArea::vertical()
            .max_height(LIST_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = ui::SPACE_XS;
                for (row, taken) in self.rows.iter_mut().zip(&taken) {
                    row.show(ui, *taken);
                }
            });
        let count = |kind: Kind| self.rows.iter().filter(|row| row.kind == kind).count();
        ui.label(ui::caption(t!(
            "admin.import_summary",
            emoji = count(Kind::Emoji),
            stickers = count(Kind::Sticker)
        )));
        let warning = match problems.first() {
            Some(Problem2::Full(sticker)) => Some(if *sticker {
                t!("admin.sticker_limit")
            } else {
                t!("admin.emoji_limit")
            }),
            Some(Problem2::PacksFull) => Some(t!("admin.pack_limit")),
            Some(Problem2::Nothing) => Some(t!("admin.import_nothing")),
            Some(Problem2::Taken(_)) => Some(t!("admin.name_taken")),
            None if self.name.trim().is_empty() => Some(t!("admin.pack_name_needed")),
            None => None,
        };
        ui::error_text(ui, warning);
        let mut out = Outcome::Open;
        ui::footer(ui, |ui| {
            let ready = problems.is_empty() && !self.name.trim().is_empty();
            let go = ui
                .add_enabled_ui(ready, |ui| ui::primary(ui, t!("admin.import_start")))
                .inner;
            if go.clicked() {
                out = Outcome::Start(self.request());
            }
            if ui::secondary(ui, t!("common.cancel")).clicked() {
                out = Outcome::Close;
            }
        });
        out
    }

    /// The upload this preview asks for: only the pictures not skipped.
    fn request(&mut self) -> Admin {
        let items: Vec<ImportItem> = std::mem::take(&mut self.rows)
            .into_iter()
            .filter(|row| row.kind != Kind::Skip)
            .map(|row| ImportItem {
                sticker: row.kind == Kind::Sticker,
                name: row.name,
                bytes: row.bytes,
            })
            .collect();
        self.stage = Stage::Uploading(0, items.len());
        Admin::ImportPack {
            name: self.name.trim().to_owned(),
            items,
        }
    }
}

impl Row {
    fn is_taken(&self, problems: &[Problem2]) -> bool {
        problems
            .iter()
            .any(|problem| matches!(problem, Problem2::Taken(name) if *name == self.name))
    }

    fn show(&mut self, ui: &mut Ui, taken: bool) {
        let skip = self.kind == Kind::Skip;
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(THUMB), egui::Sense::hover());
            if let Some(image) = &self.thumb {
                let texture = self.texture.get_or_insert_with(|| {
                    ui.ctx()
                        .load_texture("import", image.clone(), TextureOptions::LINEAR)
                });
                let tint = if skip {
                    ui::colors(ui).muted
                } else {
                    egui::Color32::WHITE
                };
                egui::Image::new(&*texture)
                    .tint(tint)
                    .fit_to_exact_size(Vec2::splat(THUMB))
                    .paint_at(ui, rect);
            }
            let kind = Vec2::new(78.0, ui::CONTROL_HEIGHT);
            let width = ui.available_width() - kind.x - ui.spacing().item_spacing.x;
            let name = ui.add_enabled(
                !skip,
                ui::field(&mut self.name)
                    .char_limit(32)
                    .desired_width(width),
            );
            if name.changed() {
                self.name = crate::emoji::clean_name(&self.name);
            }
            name.widget_info(|| {
                egui::WidgetInfo::labeled(Role::TextInput, !skip, t!("admin.import_name"))
            });
            if taken && !skip {
                ui.painter().rect_stroke(
                    name.rect,
                    ui::RADIUS,
                    egui::Stroke::new(1.5, ui::colors(ui).danger_text),
                    egui::StrokeKind::Outside,
                );
            }
            let button = ui::choice_button(ui, !skip, self.kind.label(), kind);
            button.widget_info(|| {
                egui::WidgetInfo::labeled(Role::Button, true, t!("admin.import_kind"))
            });
            if button.clicked() {
                self.kind = self.kind.next();
            }
        });
    }
}

/// What stops an import.
#[derive(Debug, PartialEq, Eq)]
enum Problem2 {
    /// More emoji (`false`) or stickers (`true`) than the server has room for.
    Full(bool),
    /// A name that is invalid or twice in the pack or already used by that kind.
    Taken(String),
    PacksFull,
    Nothing,
}

/// Everything wrong with the rows, most important first.
fn check(rows: &[Row], dir: &Directory) -> Vec<Problem2> {
    let mut problems = Vec::new();
    let kept: Vec<&Row> = rows.iter().filter(|row| row.kind != Kind::Skip).collect();
    if kept.is_empty() {
        problems.push(Problem2::Nothing);
    }
    if dir.packs.len() >= dir.packs_max {
        problems.push(Problem2::PacksFull);
    }
    for (sticker, max) in [(false, dir.emoji_max.0), (true, dir.emoji_max.1)] {
        let have = dir.emoji.iter().filter(|e| e.sticker == sticker).count();
        let adding = kept
            .iter()
            .filter(|row| (row.kind == Kind::Sticker) == sticker)
            .count();
        if have + adding > max {
            problems.push(Problem2::Full(sticker));
        }
    }
    for (at, row) in kept.iter().enumerate() {
        let sticker = row.kind == Kind::Sticker;
        let used = dir
            .emoji
            .iter()
            .any(|e| e.sticker == sticker && e.name == row.name);
        let twice = kept[..at]
            .iter()
            .any(|other| (other.kind == Kind::Sticker) == sticker && other.name == row.name);
        if !super::emoji_name_ok(&row.name) || used || twice {
            problems.push(Problem2::Taken(row.name.clone()));
        }
    }
    problems
}

/// Unpack and preview `path`; the error is for people. Also the suggested pack name,
/// from the file's name.
fn load(path: &PathBuf, max: usize) -> Loaded {
    let metadata = std::fs::metadata(path).map_err(|_| t!("admin.import_unreadable"))?;
    if metadata.len() > emoji_zip::MAX_ARCHIVE {
        return Err(t!("admin.import_too_big"));
    }
    let bytes = std::fs::read(path).map_err(|_| t!("admin.import_unreadable"))?;
    let pictures = emoji_zip::read(&bytes, max).map_err(|problem| match problem {
        Problem::NotZip => t!("admin.import_not_zip"),
        Problem::TooManyFiles | Problem::TooBig => t!("admin.import_too_big"),
        Problem::TooManyPictures(_) => t!("admin.import_too_many"),
        Problem::NoPictures => t!("admin.import_no_pictures"),
    })?;
    let rows = pictures
        .into_iter()
        .map(|picture| {
            let preview = crate::crop::preview(&picture.bytes);
            let kind = match preview {
                Some((_, side)) if side > EMOJI_SIDE => Kind::Sticker,
                _ => Kind::Emoji,
            };
            Row {
                name: picture.name,
                kind,
                thumb: preview.map(|(image, _)| image),
                texture: None,
                bytes: picture.bytes,
            }
        })
        .collect();
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().replace(['_', '-'], " "))
        .unwrap_or_default()
        .chars()
        .take(MAX_PACK_NAME)
        .collect();
    Ok((rows, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{CustomEmoji, EmojiPack};

    fn row(name: &str, kind: Kind) -> Row {
        Row {
            name: name.into(),
            kind,
            bytes: Vec::new(),
            thumb: None,
            texture: None,
        }
    }

    fn dir(emoji: &[(&str, bool)], max: (usize, usize)) -> Directory {
        Directory {
            emoji: emoji
                .iter()
                .map(|(name, sticker)| CustomEmoji {
                    id: [1; 32],
                    name: (*name).into(),
                    sticker: *sticker,
                    pack: [0; 32],
                    picture: None,
                })
                .collect(),
            emoji_max: max,
            packs: vec![EmojiPack {
                id: [0; 32],
                name: None,
            }],
            packs_max: 2,
            ..Directory::default()
        }
    }

    #[test]
    fn a_fine_import_has_no_problems_and_skips_do_not_count() {
        let rows = [row("party", Kind::Emoji), row("bad name", Kind::Skip)];
        assert!(check(&rows, &dir(&[], (5, 5))).is_empty());
    }

    #[test]
    fn limits_names_and_empty_imports_are_caught() {
        let rows = [row("a_b", Kind::Emoji), row("c_d", Kind::Emoji)];
        assert_eq!(
            check(&rows, &dir(&[("x_y", false)], (2, 5))),
            [Problem2::Full(false)]
        );
        // Stickers and emoji have separate names and limits.
        let rows = [row("hug", Kind::Sticker)];
        assert!(check(&rows, &dir(&[("hug", false)], (5, 5))).is_empty());
        assert_eq!(
            check(&rows, &dir(&[("hug", true)], (5, 5))),
            [Problem2::Taken("hug".into())]
        );
        let rows = [row("dup", Kind::Emoji), row("dup", Kind::Emoji)];
        assert_eq!(
            check(&rows, &dir(&[], (5, 5))),
            [Problem2::Taken("dup".into())]
        );
        let rows = [row("x", Kind::Emoji)];
        assert_eq!(
            check(&rows, &dir(&[], (5, 5))),
            [Problem2::Taken("x".into())]
        );
        assert_eq!(
            check(&[row("a_b", Kind::Skip)], &dir(&[], (5, 5))),
            [Problem2::Nothing]
        );
    }

    #[test]
    fn a_full_set_of_packs_stops_an_import() {
        let mut full = dir(&[], (5, 5));
        full.packs_max = 1;
        assert_eq!(
            check(&[row("party", Kind::Emoji)], &full),
            [Problem2::PacksFull]
        );
    }

    #[test]
    fn the_kind_button_cycles_through_skip() {
        assert_eq!(Kind::Emoji.next().next().next(), Kind::Emoji);
    }

    #[test]
    fn only_kept_pictures_are_sent() {
        let mut import = Import {
            stage: Stage::Ready,
            rows: vec![
                row("one", Kind::Emoji),
                row("two", Kind::Skip),
                row("three", Kind::Sticker),
            ],
            name: " Cats ".into(),
        };
        let Admin::ImportPack { name, items } = import.request() else {
            panic!("not an import");
        };
        assert_eq!(name, "Cats");
        let sent: Vec<_> = items.iter().map(|i| (i.name.as_str(), i.sticker)).collect();
        assert_eq!(sent, [("one", false), ("three", true)]);
        assert!(matches!(import.stage, Stage::Uploading(0, 2)));
    }
}
