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
    time::Duration,
};

use eframe::egui::{
    self, Align, Frame, Id as UiId, Image, Layout, Margin, Modal, Rect, Response, RichText, Role,
    ScrollArea, Sense, TextureHandle, TextureOptions, Ui, UiBuilder, Vec2, WidgetInfo,
};
use rusttalk_core::client::{Attachment, save_json};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::{
    home::frame_at,
    net::{Command, Fetch, MAX_FILE, file_name},
    server::{Chat, Id, Staged},
    ui::{self, ON_FILL, OVERLAY, OVERLAY_BUTTON, OVERLAY_TEXT},
};

/// Files per message, as the message format allows.
pub const MAX_FILES: usize = 4;
pub(crate) const THUMBNAIL: f32 = 320.0;
/// Animated GIFs and WebPs up to this size load by themselves and play in the chat.
const INLINE_MAX: u64 = 8 * MIB;
/// Animations playing in the chat at once; the ones scrolled away drop first.
const MAX_INLINE: usize = 8;
/// An animation unseen this long (seconds) may be dropped when others need the room.
const INLINE_IDLE: f64 = 1.0;
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
    /// Small looping copies of animated files shown in the chat, by file blob ID.
    inline: HashMap<Id, Inline>,
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
    /// Files pasted into a composer, read from the clipboard on their own thread.
    pastes: (mpsc::Sender<Answer>, mpsc::Receiver<Answer>),
}

/// A decoded picture: one texture, or an animation's textures and delays in ms.
/// Dropping it frees the textures.
struct Anim {
    frames: Vec<TextureHandle>,
    delays: Vec<u32>,
}

impl Anim {
    /// The texture to show now. Only a visible animation in a focused window plays and
    /// asks for repaints, so a hidden one costs nothing.
    fn current(&self, ui: &Ui, visible: bool) -> &TextureHandle {
        if visible && ui.input(|input| input.focused) && self.frames.len() > 1 {
            let now = (ui.input(|input| input.time) * 1000.0) as u64;
            if let Some((at, left)) = frame_at(self.delays.iter().copied(), now) {
                ui.ctx().request_repaint_after(Duration::from_millis(left));
                return &self.frames[at];
            }
        }
        &self.frames[0]
    }
}

struct Inline {
    picture: Picture,
    /// When it was last visible, in egui time.
    seen: f64,
}

enum Picture {
    Loading,
    Ready(Anim),
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
    /// Attach to the composer of a DM, by the other person.
    AttachDm(Id),
    /// Save a stored file.
    Save(Url, Attachment),
    /// Save a live file: transfer ID and suggested name.
    Accept(Url, Id, String),
    /// Offer a file live in a text channel or DM.
    Live(Url, Id),
    /// A new profile picture, for every server.
    Picture,
    /// A new server icon.
    Icon(Url),
    /// A new server emoji or (`true`) sticker with this name.
    Emoji(Url, String, bool),
}

pub type Answer = (Ask, Vec<PathBuf>);

impl Files {
    pub fn new(data: &Path) -> Self {
        let saved = data.join(SAVED);
        let paths = crate::load_json::<HashMap<String, PathBuf>>(&saved)
            .inspect_err(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "reading saved file places failed");
            })
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(id, path)| Some((hex::decode(id).ok()?.try_into().ok()?, path)))
            .collect();
        Self {
            images: HashMap::new(),
            inline: HashMap::new(),
            saves: HashMap::new(),
            paths,
            data: data.to_owned(),
            open_after: HashSet::new(),
            viewer: None,
            asking: false,
            answers: None,
            pastes: mpsc::channel(),
        }
    }

    pub fn progress(&mut self, id: Id, done: f32) {
        if let Some(Saving::Busy(at, _)) = self.saves.get_mut(&id) {
            *at = done;
        }
    }

    pub fn image(
        &mut self,
        ctx: &egui::Context,
        id: Id,
        fetch: Fetch,
        frames: Result<Vec<(egui::ColorImage, u32)>, bool>,
    ) {
        let slot = match fetch {
            Fetch::Inline => self.inline.get_mut(&id).map(|inline| &mut inline.picture),
            Fetch::Thumbnail | Fetch::Full => self.images.get_mut(&id),
        };
        let Some(slot) = slot else {
            // The viewer closed or the message scrolled away meanwhile.
            return;
        };
        *slot = match frames {
            Ok(frames) => {
                let delays = frames.iter().map(|(_, ms)| *ms).collect();
                let frames = (frames.into_iter())
                    .map(|(image, _)| ctx.load_texture("file", image, TextureOptions::LINEAR))
                    .collect();
                Picture::Ready(Anim { frames, delays })
            }
            Err(true) => Picture::Gone,
            Err(false) => Picture::Failed,
        };
    }

    /// Start loading the playing copy of an animated file once it is on screen, after
    /// dropping the ones scrolled away if too many are kept.
    fn want_inline(&mut self, ui: &Ui, commands: &UnboundedSender<Command>, file: &Attachment) {
        let area = Rect::from_min_size(ui.cursor().min, Vec2::splat(THUMBNAIL));
        if !ui.is_rect_visible(area) {
            return;
        }
        let now = ui.input(|input| input.time);
        if self.inline.len() >= MAX_INLINE && !self.inline.contains_key(&file.file.id) {
            self.inline.retain(|_, inline| {
                matches!(inline.picture, Picture::Loading) || now - inline.seen < INLINE_IDLE
            });
        }
        let inline = self.inline.entry(file.file.id).or_insert_with(|| {
            let _ = commands.send(Command::Image(file.file.clone(), Fetch::Inline));
            Inline {
                picture: Picture::Loading,
                seen: now,
            }
        });
        inline.seen = now;
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
        if let Err(error) = save_json(&self.data, &self.data.join(SAVED), &paths) {
            tracing::warn!(error = %format_args!("{error:#}"), "saving where files were saved failed");
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
                Ask::Attach(..)
                | Ask::AttachDm(_)
                | Ask::Live(..)
                | Ask::Picture
                | Ask::Icon(_)
                | Ask::Emoji(..) => None,
            };
            let single = !matches!(ask, Ask::Attach(..) | Ask::AttachDm(_));
            let pictures = matches!(ask, Ask::Picture | Ask::Icon(_) | Ask::Emoji(..));
            let test = *TEST_ANSWER.lock().unwrap_or_else(|e| e.into_inner());
            let paths = match (test, name) {
                (Some(answer), name) => answer(name.as_deref()),
                (None, Some(name)) => rfd::FileDialog::new()
                    .set_file_name(name)
                    .save_file()
                    .into_iter()
                    .collect(),
                (None, None) if pictures => rfd::FileDialog::new()
                    .add_filter(t!("files.pictures"), &["png", "jpg", "jpeg", "gif", "webp"])
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

    /// Read the clipboard for files to attach (see paste.rs) on its own thread, so
    /// the window keeps drawing. Without any, it becomes a text paste.
    pub fn paste(&self, ctx: &egui::Context, ask: Ask) {
        let pastes = self.pastes.0.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            match crate::paste::files() {
                Some(paths) => {
                    let _ = pastes.send((ask, paths));
                }
                None => ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste),
            }
            ctx.request_repaint();
        });
    }

    /// Dialogs answered and files pasted since the last frame.
    pub fn answers(&mut self) -> Vec<Answer> {
        let mut answers: Vec<_> = (self.answers.iter())
            .flat_map(|(_, answers)| answers.try_iter())
            .collect();
        if !answers.is_empty() {
            self.asking = false;
        }
        answers.extend(self.pastes.1.try_iter());
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
        let folder = temp_folder(&file.file.id[..8]);
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

/// A temporary folder for an opened or pasted file, by 8 bytes of ID.
pub fn temp_folder(id: &[u8]) -> PathBuf {
    std::env::temp_dir().join(format!("{OPENED}{}", hex::encode(id)))
}

/// Whether this is one of our temporary folders: `rusttalk-` and 16 hex digits,
/// so not the log folders.
fn is_temp_folder(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(OPENED))
        .is_some_and(|id| id.len() == 16 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// Remove decrypted files that "Open" left in the temporary directory, and
/// pasted pictures. Failures are only logged.
pub fn clear_opened() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if is_temp_folder(&entry.file_name())
            && let Err(error) = fs::remove_dir_all(entry.path())
        {
            tracing::warn!(%error, "removing an opened file failed");
        }
    }
}

/// Remove a pasted picture that is no longer staged. Other files stay.
pub fn discard(path: &Path) {
    let Some(folder) = path.parent() else {
        return;
    };
    if folder.parent() == Some(std::env::temp_dir().as_path())
        && folder.file_name().is_some_and(is_temp_folder)
        && let Err(error) = fs::remove_dir_all(folder)
    {
        tracing::warn!(%error, "removing a pasted picture failed");
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

/// t!("files.show_in_folder") and "Open" for a file saved on this device.
fn saved_menu(response: &Response, path: &Path) {
    ui::context_menu(response, |m| {
        if m.item("🗁", t!("files.show_in_folder")).clicked() {
            open(path, true);
        }
        if m.item("↗", t!("notify.open")).clicked() {
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
                problem = Some(t!("files.open_failed", name = name));
                continue;
            }
        };
        if size > MAX_FILE {
            problem = Some(t!("files.too_big", name = name));
            discard(&path);
        } else if chat.staged.len() >= MAX_FILES {
            problem = Some(t!("files.too_many", max = MAX_FILES));
            discard(&path);
        } else {
            chat.staged.push(Staged { path, name, size });
        }
    }
    problem
}

const KIB: u64 = 1024;
pub(crate) const MIB: u64 = KIB * 1024;
pub(crate) const GIB: u64 = MIB * 1024;

pub fn size(bytes: u64) -> String {
    match bytes {
        b if b >= GIB => format!("{} GB", crate::i18n::number(b as f64 / GIB as f64, 1)),
        b if b >= MIB => format!("{} MB", crate::i18n::number(b as f64 / MIB as f64, 1)),
        b if b >= KIB => format!("{:.0} KB", b as f64 / KIB as f64),
        b => t!("files.bytes", count = b),
    }
}

/// The staged files above the composer, each with a remove button.
pub fn staged(ui: &mut Ui, chat: &mut Chat) {
    let mut remove = None;
    ui.horizontal_wrapped(|ui| {
        for (at, file) in chat.staged.iter().enumerate() {
            Frame::new()
                .fill(ui::colors(ui).field)
                .corner_radius(ui::RADIUS)
                .inner_margin(Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.label(format!("📎 {}", file.name));
                    ui.label(ui::caption(size(file.size)));
                    if ui::small_close(ui, t!("common.remove")).clicked() {
                        remove = Some(at);
                    }
                });
        }
    });
    if let Some(at) = remove {
        discard(&chat.staged.remove(at).path);
    }
}

/// "GIF" or "WebP" for a file that may animate, by its name or type; the contents
/// decide whether it really does.
fn animated_kind(name: &str, media_type: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    if media_type == "image/gif" || name.ends_with(".gif") {
        Some("GIF")
    } else if media_type == "image/webp" || name.ends_with(".webp") {
        Some("WebP")
    } else {
        None
    }
}

/// Whether a file loads by itself and plays in the chat: it may animate and is small.
fn plays_inline(name: &str, media_type: &str, size: u64) -> bool {
    size <= INLINE_MAX && animated_kind(name, media_type).is_some()
}

/// A message's files: a preview for pictures, a card with Download for the rest.
/// Small animated GIFs and WebPs play right in the message, loaded by themselves;
/// bigger ones show their first frame with a badge and play in the viewer.
pub fn attachments(
    ui: &mut Ui,
    files: &mut Files,
    address: &Url,
    commands: &UnboundedSender<Command>,
    list: &[Attachment],
) {
    for file in list {
        let play = plays_inline(&file.name, &file.media_type, file.file.size.into());
        // Files sent before thumbnails existed for GIFs have none; they still play.
        if file.thumbnail.is_none() && !play {
            card(ui, files, address, file);
            continue;
        }
        if play {
            files.want_inline(ui, commands, file);
        }
        let inline = files
            .inline
            .get(&file.file.id)
            .map(|inline| &inline.picture);
        match inline {
            Some(Picture::Gone) => {
                gone(ui, file);
                continue;
            }
            Some(Picture::Failed) if file.thumbnail.is_none() => {
                card(ui, files, address, file);
                continue;
            }
            _ => {}
        }
        let playing = matches!(inline, Some(Picture::Ready(_)));
        if let Some(thumb) = file.thumbnail.as_ref().filter(|_| !playing) {
            if files.images.len() > MAX_PICTURES {
                let open = files.viewer.as_ref().map(|view| view.file.file.id);
                // Loading entries stay so their pending answers still land.
                files
                    .images
                    .retain(|id, picture| Some(*id) == open || matches!(picture, Picture::Loading));
            }
            files.images.entry(thumb.id).or_insert_with(|| {
                let _ = commands.send(Command::Image(thumb.clone(), Fetch::Thumbnail));
                Picture::Loading
            });
        }
        let picture = if playing {
            inline
        } else {
            (file.thumbnail.as_ref()).and_then(|thumb| files.images.get(&thumb.id))
        };
        let (mut show, mut retry) = (false, false);
        match picture {
            Some(Picture::Ready(anim)) => {
                let visible = ui
                    .is_rect_visible(Rect::from_min_size(ui.cursor().min, Vec2::splat(THUMBNAIL)));
                let shown = ui
                    .add(
                        Image::new(anim.current(ui, visible))
                            .max_size(Vec2::splat(THUMBNAIL))
                            .corner_radius(ui::RADIUS)
                            .sense(Sense::click()),
                    )
                    .on_hover_text(&file.name);
                if let Some(kind) = animated_kind(&file.name, &file.media_type).filter(|_| !playing)
                {
                    let corner = shown.rect.left_bottom() + Vec2::new(6.0, -22.0);
                    let area = Rect::from_min_size(corner, Vec2::new(60.0, 18.0));
                    ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
                        ui::badge(ui, kind, OVERLAY_BUTTON);
                    });
                }
                show = shown.clicked();
                if let Some(path) = files.saved_at(file) {
                    saved_menu(&shown, path);
                }
            }
            Some(Picture::Gone) => gone(ui, file),
            Some(Picture::Failed) => {
                retry = ui::secondary(ui, t!("files.retry_picture")).clicked();
            }
            _ => {
                ui.allocate_ui(Vec2::splat(64.0), |ui| ui.spinner());
            }
        }
        if show {
            files.images.insert(file.file.id, Picture::Loading);
            let _ = commands.send(Command::Image(file.file.clone(), Fetch::Full));
            files.viewer = Some(Viewer {
                address: address.clone(),
                file: file.clone(),
                commands: commands.clone(),
                zoom: 1.0,
                turns: 0,
            });
        }
        if retry && let Some(thumb) = &file.thumbnail {
            files.images.remove(&thumb.id);
        }
    }
}

fn gone(ui: &mut Ui, file: &Attachment) {
    ui.label(
        RichText::new(format!("📄 {}", t!("files.gone_named", name = file.name)))
            .weak()
            .italics(),
    );
}

fn card(ui: &mut Ui, files: &mut Files, address: &Url, file: &Attachment) {
    if files.saves.get(&file.file.id) == Some(&Saving::Gone) {
        gone(ui, file);
        return;
    }
    let shown = attachment_frame(ui).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("📄").size(ui::TEXT_ICON_LARGE));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(ui::bold(&file.name));
                ui.label(ui::caption(size(file.file.size.into())));
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

/// The box around a file or a live file in a message.
pub fn attachment_frame(ui: &Ui) -> Frame {
    let c = ui::colors(ui);
    Frame::new()
        .fill(c.side)
        .stroke(egui::Stroke::new(1.0, c.border))
        .corner_radius(ui::RADIUS)
        .inner_margin(Margin::same(ui::SPACE_M as i8))
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
            ui.weak(t!("call.saved"));
        }
        Some(Saving::Gone) => {
            ui.weak(t!("call.no_longer_available"));
            return;
        }
        Some(Saving::Failed) => {
            ui::error_text(ui, Some(t!("files.save_failed")));
        }
        None => {}
    }
    if ui::secondary(ui, t!("files.download")).clicked() {
        files.ask(ui.ctx(), Ask::Save(address.clone(), file.clone()));
    }
}

/// A round toolbar button in the viewer, named for screen readers.
fn tool(ui: &mut Ui, icon: &str, label: &str) -> Response {
    let button = ui
        .add(
            egui::Button::new(RichText::new(icon).size(18.0).color(ON_FILL))
                .fill(OVERLAY_BUTTON)
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
        .backdrop_color(OVERLAY)
        .show(ctx, |ui| {
            let screen = ctx.content_rect();
            ui.set_min_size(screen.size());
            ui.set_max_size(screen.size());
            Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(ui::bold(&file.name).color(ON_FILL));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = tool(ui, "×", t!("common.close")).clicked();
                        ui.add_space(12.0);
                        open_it = tool(ui, "↗", t!("files.open_default")).clicked();
                        if tool(ui, "⬇", t!("common.save")).clicked() {
                            files.ask(ui.ctx(), Ask::Save(address.clone(), file.clone()));
                        }
                        let view = files.viewer.as_mut().expect("checked above");
                        if tool(ui, "🔄", t!("files.rotate")).clicked() {
                            view.turns = (view.turns + 1) % 4;
                        }
                        if tool(ui, "➖", t!("files.zoom_out")).clicked() {
                            view.zoom = (view.zoom / 1.25).max(0.25);
                        }
                        if tool(ui, "➕", t!("files.zoom_in")).clicked() {
                            view.zoom = (view.zoom * 1.25).min(8.0);
                        }
                        match files.saves.get(&file.file.id) {
                            Some(Saving::Busy(done, _)) => {
                                ui.add(egui::ProgressBar::new(*done).desired_width(100.0));
                            }
                            Some(Saving::Done) => {
                                ui.label(RichText::new(t!("call.saved")).color(OVERLAY_TEXT));
                            }
                            Some(Saving::Failed) => {
                                ui::error_text(ui, Some(t!("files.save_failed")));
                            }
                            _ => {}
                        }
                    });
                });
                ui.add_space(8.0);
                let room = ui.available_size();
                let view = files.viewer.as_ref().expect("checked above");
                match files.images.get(&file.file.id) {
                    Some(Picture::Ready(anim)) => {
                        let texture = anim.current(ui, true);
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
                        ui.label(RichText::new(t!("call.no_longer_available")).color(OVERLAY_TEXT));
                    }
                    Some(Picture::Failed) => {
                        ui.label(RichText::new(t!("files.picture_failed")).color(OVERLAY_TEXT));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_gifs_and_webps_play_in_the_chat_big_ones_wait_for_a_click() {
        let limit = INLINE_MAX;
        assert!(plays_inline("cat.gif", "image/gif", limit));
        assert!(plays_inline("CAT.GIF", "application/octet-stream", 1));
        assert!(plays_inline("x", "image/webp", 1024));
        assert!(plays_inline("dance.WebP", "", 1024));
        assert!(!plays_inline("cat.gif", "image/gif", limit + 1));
        assert!(!plays_inline("photo.png", "image/png", 10));
        assert!(!plays_inline("notes.pdf", "application/pdf", 10));
        assert_eq!(animated_kind("a.gif", ""), Some("GIF"));
        assert_eq!(animated_kind("a.webp", ""), Some("WebP"));
        assert_eq!(animated_kind("a.png", "image/png"), None);
    }
}
