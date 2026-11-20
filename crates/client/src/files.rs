//! Files in chat: picking and dropping, previews, the picture viewer and saving.
//! Downloads and decoding run in net.rs; this only shows their state.
//! Never log file names here.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Mutex, mpsc},
    thread,
};

use eframe::egui::{
    self, Frame, Id as UiId, Image, Margin, Modal, RichText, Sense, TextureHandle, TextureOptions,
    Ui, Vec2,
};
use rusttalk_core::client::Attachment;
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::{
    net::{Command, MAX_FILE, file_name},
    server::{Chat, Id, Staged},
};

/// Files per message, as the message format allows.
const MAX_FILES: usize = 4;
const THUMBNAIL: f32 = 320.0;
/// Pictures kept before the cache is emptied; shown ones load again on demand.
const MAX_PICTURES: usize = 200;

/// Tests answer file dialogs here instead of opening one: given the suggested
/// name when saving (`None` when opening), return the chosen paths.
pub static TEST_ANSWER: Mutex<Option<Answerer>> = Mutex::new(None);
pub type Answerer = fn(Option<&str>) -> Vec<PathBuf>;

#[derive(Default)]
pub struct Files {
    /// Thumbnails and the viewer's full picture, by blob ID.
    images: HashMap<Id, Picture>,
    /// Downloads to disk, by blob ID.
    saves: HashMap<Id, Saving>,
    /// The picture open in the viewer, and its server.
    viewer: Option<(Url, Attachment)>,
    /// A file dialog is open; answers arrive on `answers`.
    asking: bool,
    answers: Option<(mpsc::Sender<Answer>, mpsc::Receiver<Answer>)>,
}

enum Picture {
    Loading,
    Ready(TextureHandle),
    Gone,
    Failed,
}

#[derive(PartialEq)]
enum Saving {
    Busy,
    Done,
    Gone,
    Failed,
}

/// Why a file dialog was opened.
pub enum Ask {
    /// Attach to the composer of a channel.
    Attach(Url, Id),
    /// Save a stored file.
    Save(Url, Attachment),
    /// Offer a file in the voice room.
    Offer(Url),
    /// Save a live file: transfer ID and suggested name.
    Accept(Url, Id, String),
}

pub type Answer = (Ask, Vec<PathBuf>);

impl Files {
    pub fn image(&mut self, ctx: &egui::Context, id: Id, image: Result<egui::ColorImage, bool>) {
        let Some(slot) = self.images.get_mut(&id) else {
            // The viewer closed meanwhile.
            return;
        };
        *slot = match image {
            Ok(image) => Picture::Ready(ctx.load_texture("file", image, TextureOptions::LINEAR)),
            Err(true) => Picture::Gone,
            Err(false) => Picture::Failed,
        };
    }

    pub fn saved(&mut self, id: Id, saved: Result<(), bool>) {
        self.saves.insert(
            id,
            match saved {
                Ok(()) => Saving::Done,
                Err(true) => Saving::Gone,
                Err(false) => Saving::Failed,
            },
        );
    }

    /// Open a file dialog on its own thread, so the window keeps drawing.
    pub fn ask(&mut self, ctx: &egui::Context, ask: Ask) {
        if self.asking {
            return;
        }
        self.asking = true;
        let answers = self.answers.get_or_insert_with(mpsc::channel).0.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let name = match &ask {
                Ask::Save(_, file) => Some(file.name.replace(['/', '\\'], "_")),
                Ask::Accept(_, _, name) => Some(name.replace(['/', '\\'], "_")),
                Ask::Attach(..) | Ask::Offer(_) => None,
            };
            let test = *TEST_ANSWER.lock().unwrap_or_else(|e| e.into_inner());
            let paths = match (test, name) {
                (Some(answer), name) => answer(name.as_deref()),
                (None, Some(name)) => rfd::FileDialog::new()
                    .set_file_name(name)
                    .save_file()
                    .into_iter()
                    .collect(),
                (None, None) => rfd::FileDialog::new().pick_files().unwrap_or_default(),
            };
            let _ = answers.send((ask, paths));
            ctx.request_repaint();
        });
    }

    /// Dialogs answered since the last frame.
    pub fn answers(&mut self) -> Vec<Answer> {
        let Some((_, answers)) = &self.answers else {
            return Vec::new();
        };
        let answers: Vec<_> = answers.try_iter().collect();
        if !answers.is_empty() {
            self.asking = false;
        }
        answers
    }

    /// Start saving a file to a chosen path.
    pub fn save(&mut self, commands: &UnboundedSender<Command>, file: &Attachment, path: PathBuf) {
        self.saves.insert(file.file.id, Saving::Busy);
        let _ = commands.send(Command::Save(file.file.clone(), path));
    }
}

/// Add picked or dropped files to the composer; returns a problem to show.
pub fn stage(chat: &mut Chat, paths: Vec<PathBuf>) -> Option<String> {
    let mut problem = None;
    for path in paths {
        let name = file_name(&path);
        let size = match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => meta.len(),
            _ => {
                problem = Some(format!("Couldn't open {name}."));
                continue;
            }
        };
        if size > MAX_FILE {
            problem = Some(format!("{name} is too big. Files can be up to 32 MB."));
        } else if chat.staged.len() >= MAX_FILES {
            problem = Some(format!("You can send up to {MAX_FILES} files at once."));
        } else {
            chat.staged.push(Staged { path, name, size });
        }
    }
    problem
}

pub fn size(bytes: u64) -> String {
    let bytes = bytes as f64;
    match bytes {
        b if b >= 1e9 => format!("{:.1} GB", b / 1e9),
        b if b >= 1e6 => format!("{:.1} MB", b / 1e6),
        b if b >= 1e3 => format!("{:.0} KB", b / 1e3),
        b => format!("{b} bytes"),
    }
}

/// The staged files above the composer, each with a remove button.
pub fn staged(ui: &mut Ui, chat: &mut Chat) {
    let mut remove = None;
    ui.horizontal_wrapped(|ui| {
        for (at, file) in chat.staged.iter().enumerate() {
            Frame::new()
                .fill(ui.visuals().faint_bg_color)
                .corner_radius(4)
                .inner_margin(Margin::symmetric(6, 3))
                .show(ui, |ui| {
                    ui.label(format!("📎 {}", file.name));
                    ui.weak(size(file.size));
                    if ui.small_button("×").on_hover_text("Remove").clicked() {
                        remove = Some(at);
                    }
                });
        }
    });
    if let Some(at) = remove {
        chat.staged.remove(at);
    }
}

/// A message's files: a preview for pictures, a card with Download for the rest.
pub fn attachments(
    ui: &mut Ui,
    files: &mut Files,
    address: &Url,
    commands: &UnboundedSender<Command>,
    list: &[Attachment],
) {
    for file in list {
        let Some(thumb) = &file.thumbnail else {
            card(ui, files, address, file);
            continue;
        };
        if files.images.len() > MAX_PICTURES {
            let open = files.viewer.as_ref().map(|(_, file)| file.file.id);
            // Loading entries stay so their pending answers still land.
            files
                .images
                .retain(|id, picture| Some(*id) == open || matches!(picture, Picture::Loading));
        }
        let picture = files.images.entry(thumb.id).or_insert_with(|| {
            let _ = commands.send(Command::Image(thumb.clone(), false));
            Picture::Loading
        });
        let (mut open, mut retry) = (false, false);
        match picture {
            Picture::Ready(texture) => {
                open = ui
                    .add(
                        Image::new(&*texture)
                            .max_size(Vec2::splat(THUMBNAIL))
                            .corner_radius(4)
                            .sense(Sense::click()),
                    )
                    .on_hover_text(&file.name)
                    .clicked();
            }
            Picture::Loading => {
                ui.allocate_ui(Vec2::splat(64.0), |ui| ui.spinner());
            }
            Picture::Gone => gone(ui, file),
            Picture::Failed => {
                retry = ui.small_button("Couldn't load picture. Retry").clicked();
            }
        }
        if open {
            files.images.insert(file.file.id, Picture::Loading);
            let _ = commands.send(Command::Image(file.file.clone(), true));
            files.viewer = Some((address.clone(), file.clone()));
        }
        if retry {
            files.images.remove(&thumb.id);
        }
    }
}

fn gone(ui: &mut Ui, file: &Attachment) {
    ui.label(
        RichText::new(format!("📄 {} · No longer available", file.name))
            .weak()
            .italics(),
    );
}

fn card(ui: &mut Ui, files: &mut Files, address: &Url, file: &Attachment) {
    if files.saves.get(&file.file.id) == Some(&Saving::Gone) {
        gone(ui, file);
        return;
    }
    Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(6)
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("📄").size(22.0));
                ui.vertical(|ui| {
                    ui.label(RichText::new(&file.name).strong());
                    ui.weak(size(file.file.size.into()));
                });
                ui.add_space(12.0);
                download(ui, files, address, file);
            });
        });
}

/// Download, or how the last download went.
fn download(ui: &mut Ui, files: &mut Files, address: &Url, file: &Attachment) {
    match files.saves.get(&file.file.id) {
        Some(Saving::Busy) => {
            ui.spinner();
            ui.weak("Saving…");
            return;
        }
        Some(Saving::Done) => {
            ui.weak("Saved");
        }
        Some(Saving::Gone) => {
            ui.weak("No longer available");
            return;
        }
        Some(Saving::Failed) => {
            ui.colored_label(ui.visuals().error_fg_color, "Couldn't save.");
        }
        None => {}
    }
    if ui.button("Download").clicked() {
        files.ask(ui.ctx(), Ask::Save(address.clone(), file.clone()));
    }
}

/// The picture viewer, over everything else.
pub fn viewer(ctx: &egui::Context, files: &mut Files) {
    let Some((address, file)) = files.viewer.clone() else {
        return;
    };
    let mut close = false;
    let modal = Modal::new(UiId::unique("viewer")).show(ctx, |ui| {
        let room = ctx.content_rect().size() * 0.8;
        ui.label(RichText::new(&file.name).strong());
        match files.images.get(&file.file.id) {
            Some(Picture::Ready(texture)) => {
                ui.add(Image::new(texture).max_size(room));
            }
            Some(Picture::Gone) => {
                ui.weak("No longer available");
            }
            Some(Picture::Failed) => {
                ui.weak("Couldn't open this picture. You can still download it.");
            }
            _ => {
                ui.allocate_ui(Vec2::new(240.0, 160.0), |ui| ui.spinner());
            }
        }
        ui.horizontal(|ui| {
            download(ui, files, &address, &file);
            close = ui.button("Close").clicked();
        });
    });
    if close || modal.should_close() {
        // The full picture can be large; keep only thumbnails around.
        files.images.remove(&file.file.id);
        files.viewer = None;
    }
}
