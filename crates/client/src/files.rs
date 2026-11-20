//! Files in chat: picking and dropping, previews, the picture viewer and saving.
//! Downloads and decoding run in net.rs; this only shows their state.
//! Never log file names here.

use std::{
    collections::{HashMap, HashSet},
    f32::consts::FRAC_PI_2,
    fs, io,
    path::{Path, PathBuf},
    sync::{Mutex, mpsc},
    thread,
};

use eframe::egui::{
    self, Align, Color32, Frame, Id as UiId, Image, Layout, Margin, Modal, Rect, Response,
    RichText, Role, ScrollArea, Sense, TextureHandle, TextureOptions, Ui, Vec2, WidgetInfo,
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

/// Tests get the path here instead of the file manager or an app opening:
/// the path, and whether its folder was asked for.
pub static TEST_OPEN: Mutex<Option<Opener>> = Mutex::new(None);
pub type Opener = fn(&Path, bool);

pub struct Files {
    /// Thumbnails and the viewer's full picture, by blob ID.
    images: HashMap<Id, Picture>,
    /// Downloads to disk, by blob ID.
    saves: HashMap<Id, Saving>,
    /// Where stored files were saved on this device, by blob ID. Kept in `saved.json`
    /// in the profile and never synced or sent.
    paths: HashMap<Id, PathBuf>,
    /// The profile folder.
    data: PathBuf,
    /// Downloads to open once saved, instead of remembering.
    open_after: HashSet<Id>,
    viewer: Option<Viewer>,
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
    /// How far (0 to 1), and where to.
    Busy(f32, PathBuf),
    Done,
    Gone,
    Failed,
}

/// The picture open in the viewer.
struct Viewer {
    address: Url,
    file: Attachment,
    commands: UnboundedSender<Command>,
    zoom: f32,
    /// Quarter turns clockwise.
    turns: u8,
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
    /// Offer a file live in a text channel or DM.
    Live(Url, Id),
    /// A new profile picture, for every server.
    Picture,
    /// A new server icon.
    Icon(Url),
}

pub type Answer = (Ask, Vec<PathBuf>);

impl Files {
    pub fn new(data: &Path) -> Self {
        let saved = data.join(SAVED);
        let paths = match fs::read(&saved) {
            Ok(bytes) => serde_json::from_slice::<HashMap<String, PathBuf>>(&bytes)
                .inspect_err(|error| tracing::warn!(%error, "reading saved file places failed"))
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(id, path)| Some((hex::decode(id).ok()?.try_into().ok()?, path)))
                .collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => {
                tracing::warn!(%error, "reading saved file places failed");
                HashMap::new()
            }
        };
        Self {
            images: HashMap::new(),
            saves: HashMap::new(),
            paths,
            data: data.to_owned(),
            open_after: HashSet::new(),
            viewer: None,
            asking: false,
            answers: None,
        }
    }

    pub fn progress(&mut self, id: Id, done: f32) {
        if let Some(Saving::Busy(at, _)) = self.saves.get_mut(&id) {
            *at = done;
        }
    }

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
        let path = match self.saves.remove(&id) {
            Some(Saving::Busy(_, path)) => Some(path),
            _ => None,
        };
        let state = match saved {
            Ok(()) => {
                if let Some(path) = path {
                    if self.open_after.remove(&id) {
                        open(&path, false);
                    } else {
                        self.paths.insert(id, path);
                        self.store();
                    }
                }
                Saving::Done
            }
            Err(true) => Saving::Gone,
            Err(false) => Saving::Failed,
        };
        self.open_after.remove(&id);
        self.saves.insert(id, state);
    }

    fn store(&self) {
        let paths: HashMap<String, &PathBuf> = (self.paths.iter())
            .map(|(id, path)| (hex::encode(id), path))
            .collect();
        let written = serde_json::to_vec(&paths)
            .map_err(io::Error::other)
            .and_then(|bytes| fs::write(self.data.join(SAVED), bytes));
        if let Err(error) = written {
            tracing::warn!(%error, "saving where files were saved failed");
        }
    }

    /// Where this stored file was saved, if it is still there.
    fn saved_at(&self, file: &Attachment) -> Option<&PathBuf> {
        self.paths.get(&file.file.id).filter(|path| path.is_file())
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
                Ask::Attach(..) | Ask::Offer(_) | Ask::Live(..) | Ask::Picture | Ask::Icon(_) => {
                    None
                }
            };
            let single = !matches!(ask, Ask::Attach(..));
            let pictures = matches!(ask, Ask::Picture | Ask::Icon(_));
            let test = *TEST_ANSWER.lock().unwrap_or_else(|e| e.into_inner());
            let paths = match (test, name) {
                (Some(answer), name) => answer(name.as_deref()),
                (None, Some(name)) => rfd::FileDialog::new()
                    .set_file_name(name)
                    .save_file()
                    .into_iter()
                    .collect(),
                (None, None) if pictures => rfd::FileDialog::new()
                    .add_filter("Pictures", &["png", "jpg", "jpeg", "gif"])
                    .pick_file()
                    .into_iter()
                    .collect(),
                (None, None) if single => rfd::FileDialog::new().pick_file().into_iter().collect(),
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
        self.saves
            .insert(file.file.id, Saving::Busy(0.0, path.clone()));
        let _ = commands.send(Command::Save(file.file.clone(), path));
    }

    /// Open a stored file in its app: the saved copy, or else a fresh one in a temporary folder.
    fn open_file(&mut self, commands: &UnboundedSender<Command>, file: &Attachment) {
        if let Some(path) = self.saved_at(file) {
            open(path, false);
            return;
        }
        if matches!(self.saves.get(&file.file.id), Some(Saving::Busy(..))) {
            return;
        }
        let folder =
            std::env::temp_dir().join(format!("{OPENED}{}", hex::encode(&file.file.id[..8])));
        if let Err(error) = fs::create_dir_all(&folder) {
            tracing::warn!(%error, "making a folder to open a file failed");
            self.saves.insert(file.file.id, Saving::Failed);
            return;
        }
        self.open_after.insert(file.file.id);
        let name = file.name.replace(['/', '\\'], "_");
        self.save(commands, file, folder.join(name));
    }
}

const SAVED: &str = "saved.json";
/// Prefix of the temporary folders that "Open" writes decrypted files to.
const OPENED: &str = "rusttalk-";

/// Remove decrypted files that "Open" left in the temporary directory
/// (`rusttalk-` and 16 hex digits, so not the log folders). Failures are only logged.
pub fn clear_opened() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_prefix(OPENED)) else {
            continue;
        };
        if id.len() == 16
            && id.bytes().all(|byte| byte.is_ascii_hexdigit())
            && let Err(error) = fs::remove_dir_all(entry.path())
        {
            tracing::warn!(%error, "removing an opened file failed");
        }
    }
}

/// Open a file in its app, or (`folder`) show it in the file manager. Failures are only logged.
fn open(path: &Path, folder: bool) {
    let hook = *TEST_OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hook) = hook {
        hook(path, folder);
        return;
    }
    let opened = if folder {
        show_in_folder(path)
    } else {
        open::that_detached(path)
    };
    if let Err(error) = opened {
        tracing::warn!(%error, folder, "opening a saved file failed");
    }
}

#[cfg(windows)]
fn show_in_folder(path: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    // Explorer selects the file; its exit code means nothing.
    std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(drop)
}

/// Opens the folder; selecting the file differs per Linux file manager.
#[cfg(not(windows))]
fn show_in_folder(path: &Path) -> io::Result<()> {
    open::that_detached(path.parent().unwrap_or(path))
}

/// "Show in folder" and "Open" for a file saved on this device.
fn saved_menu(response: &Response, path: &Path) {
    response.context_menu(|ui| {
        if ui.button("Show in folder").clicked() {
            open(path, true);
        }
        if ui.button("Open").clicked() {
            open(path, false);
        }
    });
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
            let open = files.viewer.as_ref().map(|view| view.file.file.id);
            // Loading entries stay so their pending answers still land.
            files
                .images
                .retain(|id, picture| Some(*id) == open || matches!(picture, Picture::Loading));
        }
        let picture = files.images.entry(thumb.id).or_insert_with(|| {
            let _ = commands.send(Command::Image(thumb.clone(), false));
            Picture::Loading
        });
        let (mut show, mut retry) = (false, false);
        match picture {
            Picture::Ready(texture) => {
                let shown = ui
                    .add(
                        Image::new(&*texture)
                            .max_size(Vec2::splat(THUMBNAIL))
                            .corner_radius(4)
                            .sense(Sense::click()),
                    )
                    .on_hover_text(&file.name);
                show = shown.clicked();
                if let Some(path) = files.saved_at(file) {
                    saved_menu(&shown, path);
                }
            }
            Picture::Loading => {
                ui.allocate_ui(Vec2::splat(64.0), |ui| ui.spinner());
            }
            Picture::Gone => gone(ui, file),
            Picture::Failed => {
                retry = ui.small_button("Couldn't load picture. Retry").clicked();
            }
        }
        if show {
            files.images.insert(file.file.id, Picture::Loading);
            let _ = commands.send(Command::Image(file.file.clone(), true));
            files.viewer = Some(Viewer {
                address: address.clone(),
                file: file.clone(),
                commands: commands.clone(),
                zoom: 1.0,
                turns: 0,
            });
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
    let shown = Frame::new()
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
    if let Some(path) = files.saved_at(file) {
        let card = shown.response.interact(Sense::click());
        card.widget_info(|| WidgetInfo::labeled(Role::Button, true, &file.name));
        saved_menu(&card, path);
    }
}

/// Download, or how the last download went.
fn download(ui: &mut Ui, files: &mut Files, address: &Url, file: &Attachment) {
    match files.saves.get(&file.file.id) {
        Some(Saving::Busy(done, _)) => {
            ui.add(
                egui::ProgressBar::new(*done)
                    .show_percentage()
                    .desired_width(120.0),
            );
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

/// A round toolbar button in the viewer, named for screen readers.
fn tool(ui: &mut Ui, icon: &str, label: &str) -> Response {
    let button = ui
        .add(
            egui::Button::new(RichText::new(icon).size(18.0).color(Color32::WHITE))
                .fill(Color32::from_black_alpha(160))
                .corner_radius(18)
                .min_size(Vec2::splat(36.0)),
        )
        .on_hover_text(label);
    button.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    button
}

/// The picture viewer over the whole window: zoom, rotate, save, open, close (also Esc).
pub fn viewer(ctx: &egui::Context, files: &mut Files) {
    let Some(view) = files.viewer.as_ref() else {
        return;
    };
    let (address, file) = (view.address.clone(), view.file.clone());
    let commands = view.commands.clone();
    let (mut close, mut open_it) = (false, false);
    let modal = Modal::new(UiId::unique("viewer"))
        .frame(Frame::NONE)
        .backdrop_color(Color32::from_black_alpha(225))
        .show(ctx, |ui| {
            let screen = ctx.content_rect();
            ui.set_min_size(screen.size());
            ui.set_max_size(screen.size());
            Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&file.name).strong().color(Color32::WHITE));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = tool(ui, "×", "Close").clicked();
                        ui.add_space(12.0);
                        open_it = tool(ui, "↗", "Open in default app").clicked();
                        if tool(ui, "⬇", "Save").clicked() {
                            files.ask(ui.ctx(), Ask::Save(address.clone(), file.clone()));
                        }
                        let view = files.viewer.as_mut().expect("checked above");
                        if tool(ui, "🔄", "Rotate").clicked() {
                            view.turns = (view.turns + 1) % 4;
                        }
                        if tool(ui, "➖", "Zoom out").clicked() {
                            view.zoom = (view.zoom / 1.25).max(0.25);
                        }
                        if tool(ui, "➕", "Zoom in").clicked() {
                            view.zoom = (view.zoom * 1.25).min(8.0);
                        }
                        match files.saves.get(&file.file.id) {
                            Some(Saving::Busy(done, _)) => {
                                ui.add(egui::ProgressBar::new(*done).desired_width(100.0));
                            }
                            Some(Saving::Done) => {
                                ui.label(RichText::new("Saved").color(Color32::LIGHT_GRAY));
                            }
                            Some(Saving::Failed) => {
                                ui.colored_label(ui.visuals().error_fg_color, "Couldn't save.");
                            }
                            _ => {}
                        }
                    });
                });
                ui.add_space(8.0);
                let room = ui.available_size();
                let view = files.viewer.as_ref().expect("checked above");
                match files.images.get(&file.file.id) {
                    Some(Picture::Ready(texture)) => {
                        let size = texture.size_vec2();
                        let turned = view.turns % 2 == 1;
                        let outline = if turned {
                            Vec2::new(size.y, size.x)
                        } else {
                            size
                        };
                        let fit = (room.x / outline.x).min(room.y / outline.y).min(1.0);
                        let scale = fit * view.zoom;
                        let area = (outline * scale).max(room);
                        ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                            let (rect, _) = ui.allocate_exact_size(area, Sense::hover());
                            let unturned = Rect::from_center_size(rect.center(), size * scale);
                            Image::new(texture)
                                .rotate(f32::from(view.turns) * FRAC_PI_2, Vec2::splat(0.5))
                                .paint_at(ui, unturned);
                        });
                    }
                    Some(Picture::Gone) => {
                        ui.label(RichText::new("No longer available").color(Color32::LIGHT_GRAY));
                    }
                    Some(Picture::Failed) => {
                        ui.label(
                            RichText::new("Couldn't open this picture. You can still save it.")
                                .color(Color32::LIGHT_GRAY),
                        );
                    }
                    _ => {
                        ui.centered_and_justified(|ui| ui.spinner());
                    }
                }
            });
        });
    if open_it {
        files.open_file(&commands, &file);
    }
    if close || modal.should_close() {
        // The full picture can be large; keep only thumbnails around.
        files.images.remove(&file.file.id);
        files.viewer = None;
    }
}
