//! Files in chat: picking and dropping, previews, the picture viewer and saving.
//! Downloads and decoding run in net.rs; this only shows their state.
//! Never log file names here.

use std::{
    collections::{HashMap, HashSet},
    f32::consts::FRAC_PI_2,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
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
    link_cards,
    net::{Command, Fetch, MAX_FILE, file_name},
    server::{Chat, Id, Picture as Picture2, Staged},
    sticker_files,
    ui::{self, ON_FILL, OVERLAY, OVERLAY_BUTTON, OVERLAY_TEXT},
};

/// Whether this device has a file dialog. Phones need the system's own picker first
/// (docs/mobile.md); until then the buttons that open one are off there.
pub const PICKER: bool = !cfg!(any(target_os = "android", target_os = "ios"));

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
/// Decoded bytes all received emoji and sticker files may hold together.
const STICKER_BUDGET: usize = 256 * 1024 * 1024;

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
    /// Link previews sent along with messages, by file blob ID.
    cards: HashMap<Id, Card>,
    /// Emoji and sticker files of other servers that were loaded, oldest first, with the
    /// bytes each holds decoded, and the total; their pictures are in egui's memory
    /// (see `sticker_files`).
    stickers: std::collections::VecDeque<(Id, usize)>,
    sticker_bytes: usize,
}

/// A link preview file as loaded.
enum Card {
    Loading,
    Ready(link_cards::Card),
    /// Gone or unreadable: nothing is shown, as for a message without one.
    Missing,
}

/// Whether a file is a link preview, which apps that know the type show as a card
/// instead of a file (see `link_cards`).
pub fn is_card(file: &Attachment) -> bool {
    file.media_type == link_cards::MEDIA_TYPE && file.file.size <= link_cards::MAX_CARD_BYTES
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
    /// Add pictures to a page channel that is being edited.
    PagePictures(Url, Id),
    /// Save a stored file.
    Save(Url, Attachment),
    /// Upload into a channel's shared files.
    Share(Url, Id),
    /// Save a live file: transfer ID and suggested name.
    Accept(Url, Id, String),
    /// Offer a file live in a text channel or DM.
    Live(Url, Id),
    /// A new profile picture, for every server.
    Picture,
    /// A new server icon.
    Icon(Url),
    /// A new server emoji or (`true`) sticker with this name, for this pack (the first
    /// when `None`).
    Emoji(Url, String, bool, Option<Id>),
    /// A .zip of pictures to import as an emoji pack.
    EmojiZip(Url),
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
            cards: HashMap::new(),
            stickers: Default::default(),
            sticker_bytes: 0,
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

    /// An emoji or sticker file of another server finished loading. The oldest pictures
    /// go when together they hold more than `STICKER_BUDGET` decoded bytes; they load
    /// again when shown.
    pub fn sticker_loaded(&mut self, ctx: &egui::Context, id: Id, picture: Result<Picture2, bool>) {
        let (state, bytes) = match picture {
            Ok(picture) => {
                let bytes = sticker_files::decoded_bytes(&picture);
                (sticker_files::State::Ready(picture), bytes)
            }
            Err(_) => (sticker_files::State::Failed, 0),
        };
        sticker_files::set(ctx, &id, state);
        self.stickers.push_back((id, bytes));
        self.sticker_bytes += bytes;
        for old in evict(&mut self.stickers, &mut self.sticker_bytes, STICKER_BUDGET) {
            sticker_files::forget(ctx, &old);
        }
    }

    /// The address of a loaded link preview file.
    pub fn card_url(&self, id: &Id) -> Option<Url> {
        match self.cards.get(id)? {
            Card::Ready(card) => link_cards::web_url(&card.url),
            _ => None,
        }
    }

    pub fn link_card(&mut self, id: Id, card: Result<link_cards::Card, bool>) {
        if let Some(slot) = self.cards.get_mut(&id) {
            *slot = card.map_or(Card::Missing, Card::Ready);
        }
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
                | Ask::PagePictures(..)
                | Ask::Share(..)
                | Ask::Live(..)
                | Ask::Picture
                | Ask::Icon(_)
                | Ask::Emoji(..)
                | Ask::EmojiZip(_) => None,
            };
            let single = !matches!(
                ask,
                Ask::Attach(..) | Ask::AttachDm(_) | Ask::PagePictures(..) | Ask::Share(..)
            );
            let pictures = matches!(
                ask,
                Ask::Picture | Ask::Icon(_) | Ask::Emoji(..) | Ask::PagePictures(..)
            );
            let zip = matches!(ask, Ask::EmojiZip(_));
            let test = *TEST_ANSWER.lock().unwrap_or_else(|e| e.into_inner());
            let paths = match (test, name) {
                (Some(answer), name) => answer(name.as_deref()),
                // shortcut: phones need the system file picker (docs/mobile.md); until
                // then a pick finds nothing.
                #[cfg(any(target_os = "android", target_os = "ios"))]
                (None, _) => {
                    tracing::warn!("no file picker on this device yet");
                    let _ = (single, pictures, zip);
                    Vec::new()
                }
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                (None, Some(name)) => rfd::FileDialog::new()
                    .set_file_name(name)
                    .save_file()
                    .into_iter()
                    .collect(),
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                (None, None) if pictures => {
                    let dialog = rfd::FileDialog::new()
                        .add_filter(t!("files.pictures"), &["png", "jpg", "jpeg", "gif", "webp"]);
                    if single {
                        dialog.pick_file().into_iter().collect()
                    } else {
                        dialog.pick_files().unwrap_or_default()
                    }
                }
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                (None, None) if zip => rfd::FileDialog::new()
                    .add_filter(t!("files.zip"), &["zip"])
                    .pick_file()
                    .into_iter()
                    .collect(),
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                (None, None) if single => rfd::FileDialog::new().pick_file().into_iter().collect(),
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
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
                None => crate::paste::request_text(&ctx),
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

/// Finder opens the folder with the file selected.
#[cfg(target_os = "macos")]
fn show_in_folder(path: &Path) -> io::Result<()> {
    std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map(drop)
}

/// Opens the folder; selecting the file differs per Linux file manager.
#[cfg(not(any(windows, target_os = "macos")))]
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
            problem = Some(too_big(&name, size));
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

/// The local stored-file cap also applies before the Files tab starts an upload.
pub(crate) fn too_big(name: &str, bytes: u64) -> String {
    t!(
        "files.too_big",
        name = name,
        size = size(bytes),
        limit = size(MAX_FILE)
    )
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

/// The side of a staged picture's tile in the composer.
const PREVIEW: f32 = 112.0;
/// Decoded memory a staged picture's tile and its preview may use.
const PREVIEW_BUDGET: usize = 32 * MIB as usize;
const OPEN_BUDGET: usize = 64 * MIB as usize;
const OPEN_EDGE: u32 = 1600;

/// Pictures decoding at once; the rest wait their turn.
const MAX_DECODES: usize = 2;

/// A staged picture's preview: its path, whether it is the large copy for the preview
/// dialog, and the file's size and change time, so a changed file decodes again.
type PreviewKey = (PathBuf, bool, u64, Option<std::time::SystemTime>);

/// Staged pictures' previews, decoded on their own threads, at most `MAX_DECODES`
/// at once. Entries go when the file is no longer staged, and at sign-out.
#[derive(Clone, Default)]
struct Previews(Arc<Mutex<HashMap<PreviewKey, Preview>>>);

enum Preview {
    Waiting,
    /// Decoding; clearing the flag stops it before the next frame.
    Loading(Arc<AtomicBool>),
    Decoded(Vec<(egui::ColorImage, u32)>),
    Ready(Anim),
    Failed,
}

impl Drop for Preview {
    fn drop(&mut self) {
        if let Preview::Loading(going) = self {
            going.store(false, Ordering::Relaxed);
        }
    }
}

fn is_picture(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|end| name.ends_with(end))
}

fn preview_key(path: &Path, large: bool) -> PreviewKey {
    let meta = fs::metadata(path).ok();
    let size = meta.as_ref().map_or(0, |meta| meta.len());
    let changed = meta.and_then(|meta| meta.modified().ok());
    (path.to_owned(), large, size, changed)
}

/// Decode a staged picture to fit `edge`, with every frame of an animation.
fn decode_staged(
    path: &Path,
    edge: u32,
    budget: usize,
    going: &AtomicBool,
) -> anyhow::Result<Vec<(egui::ColorImage, u32)>> {
    let bytes = fs::read(path)?;
    let frames = rusttalk_core::client::decode_frames_until(&bytes, Some(edge), budget, || {
        going.load(Ordering::Relaxed)
    })?;
    Ok(frames.into_iter().map(crate::net::frame_image).collect())
}

impl Previews {
    fn get(ctx: &egui::Context) -> Self {
        ctx.data_mut(|data| {
            data.get_temp_mut_or_default::<Self>(UiId::unique("staged-previews"))
                .clone()
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PreviewKey, Preview>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start waiting decodes while there is room.
    fn start(&self, ctx: &egui::Context, map: &mut HashMap<PreviewKey, Preview>) {
        let mut busy = (map.values())
            .filter(|preview| matches!(preview, Preview::Loading(_)))
            .count();
        for (key, preview) in map.iter_mut() {
            if busy >= MAX_DECODES {
                break;
            }
            if !matches!(preview, Preview::Waiting) {
                continue;
            }
            busy += 1;
            let going = Arc::new(AtomicBool::new(true));
            *preview = Preview::Loading(going.clone());
            let (previews, ctx, key) = (self.clone(), ctx.clone(), key.clone());
            thread::spawn(move || {
                let (edge, budget) = if key.1 {
                    (OPEN_EDGE, OPEN_BUDGET)
                } else {
                    (PREVIEW as u32 * 2, PREVIEW_BUDGET)
                };
                let decoded = decode_staged(&key.0, edge, budget, &going);
                let mut map = previews.lock();
                // Unstaged or changed meanwhile: nothing to show it in.
                if let Some(slot) = map.get_mut(&key)
                    && matches!(slot, Preview::Loading(_))
                {
                    *slot = match decoded {
                        Ok(frames) => Preview::Decoded(frames),
                        Err(error) => {
                            tracing::warn!(error = %format_args!("{error:#}"), "previewing a picture failed");
                            Preview::Failed
                        }
                    };
                }
                // The next waiting one starts on the next frame.
                ctx.request_repaint();
            });
        }
    }

    /// The picture's frames as textures once decoded; queues it on first ask.
    fn with<R>(
        &self,
        ui: &mut Ui,
        path: &Path,
        large: bool,
        show: impl FnOnce(&mut Ui, Option<&Anim>, bool) -> R,
    ) -> R {
        let key = preview_key(path, large);
        let mut map = self.lock();
        // A changed file at the same path: the old preview no longer applies.
        map.retain(|other, _| other.0 != key.0 || other.1 != large || *other == key);
        map.entry(key.clone()).or_insert(Preview::Waiting);
        self.start(ui.ctx(), &mut map);
        let entry = map.get_mut(&key).expect("inserted above");
        if let Preview::Decoded(frames) = entry {
            let delays = frames.iter().map(|(_, ms)| *ms).collect();
            let frames = (std::mem::take(frames).into_iter())
                .map(|(image, _)| {
                    ui.ctx()
                        .load_texture("staged", image, TextureOptions::LINEAR)
                })
                .collect();
            *entry = Preview::Ready(Anim { frames, delays });
        }
        match entry {
            Preview::Ready(anim) => show(ui, Some(anim), false),
            Preview::Failed => show(ui, None, true),
            _ => show(ui, None, false),
        }
    }

    /// Drop the previews of files no longer staged, stopping their decodes.
    fn keep(&self, staged: &[Staged]) {
        self.lock()
            .retain(|(path, ..), _| staged.iter().any(|file| file.path == *path));
    }
}

/// Forget every staged picture's preview, as at sign-out.
pub fn forget_previews(ctx: &egui::Context) {
    Previews::get(ctx).lock().clear();
}

/// Drop previews of files no longer in this composer, also once it holds none.
pub fn keep_previews(ctx: &egui::Context, staged: &[Staged]) {
    Previews::get(ctx).keep(staged);
    if staged.is_empty() {
        ctx.data_mut(|data| data.remove::<PathBuf>(UiId::unique("staged-open")));
    }
}

/// The staged files in the composer: pictures as tiles that play on hover and open
/// large on click, other files as a chip with the name and size. Each has a remove button.
pub fn staged(ui: &mut Ui, chat: &mut Chat) {
    let previews = Previews::get(ui.ctx());
    let open_id = UiId::unique("staged-open");
    previews.keep(&chat.staged);
    let mut remove = None;
    let mut open = None;
    ui.horizontal_wrapped(|ui| {
        for (at, file) in chat.staged.iter().enumerate() {
            let tile = is_picture(&file.name)
                && previews.with(ui, &file.path, false, |ui, anim, failed| {
                    if failed {
                        return false;
                    }
                    let (shown, removed) = picture_tile(ui, file, anim);
                    open = open.or(shown.then_some(at));
                    remove = remove.or(removed.then_some(at));
                    true
                });
            if tile {
                continue;
            }
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
    if let Some(at) = open {
        ui.data_mut(|data| data.insert_temp(open_id, chat.staged[at].path.clone()));
    }
    let opened: Option<PathBuf> = ui.data(|data| data.get_temp(open_id));
    if let Some(path) = opened {
        let file = chat.staged.iter().find(|file| file.path == path);
        let close = match file {
            Some(file) => staged_dialog(ui, &previews, file),
            None => true,
        };
        if close {
            previews.lock().retain(|key, _| key.0 != path || !key.1);
            ui.data_mut(|data| data.remove::<PathBuf>(open_id));
        }
    }
    if let Some(at) = remove {
        discard(&chat.staged.remove(at).path);
    }
}

/// A staged picture's tile: cropped to a square, playing while hovered, with a ×.
/// Returns whether it was clicked and whether × was.
fn picture_tile(ui: &mut Ui, file: &Staged, anim: Option<&Anim>) -> (bool, bool) {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(PREVIEW), Sense::click());
    let c = ui::colors(ui);
    ui.painter().rect_filled(rect, ui::RADIUS, c.field);
    match anim {
        Some(anim) => {
            let texture = anim.current(ui, response.hovered());
            let size = texture.size_vec2();
            // Cover the square: crop the longer side around the middle.
            let cut = (Vec2::splat(size.min_elem()) / size).min(Vec2::splat(1.0));
            let uv = Rect::from_center_size(egui::pos2(0.5, 0.5), cut);
            Image::new(texture)
                .uv(uv)
                .corner_radius(ui::RADIUS)
                .paint_at(ui, rect);
            if anim.frames.len() > 1
                && let Some(kind) = animated_kind(&file.name, "")
            {
                let corner = rect.left_bottom() + Vec2::new(6.0, -22.0);
                let area = Rect::from_min_size(corner, Vec2::new(60.0, 18.0));
                ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
                    ui::badge(ui, kind, OVERLAY_BUTTON);
                });
            }
        }
        None => {
            ui.put(rect, egui::Spinner::new());
        }
    }
    ui::focus_ring(ui, &response, ui::RADIUS);
    let label = format!("{} · {}", file.name, size(file.size));
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
    let shown = response
        .on_hover_text(&label)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked();
    let corner = Rect::from_min_size(rect.right_top() + Vec2::new(-26.0, 4.0), Vec2::splat(22.0));
    let removed = ui
        .scope_builder(UiBuilder::new().max_rect(corner), |ui| {
            ui::icon_button(
                ui,
                "×",
                t!("common.remove"),
                ui::Look::On(OVERLAY_BUTTON),
                Vec2::splat(22.0),
            )
        })
        .inner
        .clicked();
    (shown, removed)
}

/// A staged picture large, before it is sent. True when it should close.
fn staged_dialog(ui: &Ui, previews: &Previews, file: &Staged) -> bool {
    let ((), close) = ui::dialog(ui.ctx(), "staged-preview", &file.name, |ui| {
        let room = Vec2::new(ui.available_width(), 420.0);
        previews.with(ui, &file.path, true, |ui, anim, failed| match anim {
            Some(anim) => {
                let texture = anim.current(ui, true);
                let size = texture.size_vec2();
                let scale = (room.x / size.x).min(room.y / size.y).min(1.0);
                ui.vertical_centered(|ui| {
                    ui.add(
                        Image::new(texture)
                            .fit_to_exact_size(size * scale)
                            .corner_radius(ui::RADIUS),
                    );
                });
            }
            None if failed => {
                ui.label(t!("files.picture_failed"));
            }
            None => {
                ui.allocate_ui(Vec2::new(room.x, 120.0), |ui| {
                    ui.centered_and_justified(|ui| ui.spinner())
                });
            }
        });
        ui.label(ui::caption(size(file.size)));
    });
    close
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
/// A message's files; `links` are its text's link destinations, which a link preview
/// must be about.
pub fn attachments(
    ui: &mut Ui,
    files: &mut Files,
    (address, commands): (&Url, &UnboundedSender<Command>),
    list: &[Attachment],
    links: &[String],
) {
    let (cards, list): (Vec<&Attachment>, Vec<&Attachment>) =
        list.iter().partition(|file| is_card(file));
    // Emoji and stickers from other servers are drawn inline, not as files.
    let (pictures, list): (Vec<&Attachment>, Vec<&Attachment>) = list
        .into_iter()
        .partition(|file| sticker_files::of(file).is_some());
    for file in pictures {
        sticker_file(ui, files, (address, commands), file);
    }
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
            bound_pictures(
                &mut files.images,
                files.viewer.as_ref().map(|v| v.file.file.id),
            );
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
    // One preview per message, as senders make.
    if let Some(card) = cards.first() {
        link_card(ui, files, commands, card, links);
    }
}

/// An emoji or sticker file of another server: loaded when on screen. A sticker is drawn
/// here; an emoji is drawn by the message text that uses it. A file that fails the checks
/// shows as an ordinary file.
fn sticker_file(
    ui: &mut Ui,
    files: &mut Files,
    (address, commands): (&Url, &UnboundedSender<Command>),
    file: &Attachment,
) {
    let Some((kind, name)) = sticker_files::of(file) else {
        return;
    };
    let id = file.file.id;
    let area = Rect::from_min_size(ui.cursor().min, Vec2::splat(crate::home::STICKER_SIZE));
    if sticker_files::state(ui.ctx(), &id).is_none() && ui.is_rect_visible(area) {
        sticker_files::set(ui.ctx(), &id, sticker_files::State::Loading);
        let _ = commands.send(Command::StickerFile(file.file.clone(), kind));
    }
    match sticker_files::state(ui.ctx(), &id) {
        Some(sticker_files::State::Failed) => card(ui, files, address, file),
        Some(sticker_files::State::Ready(picture)) if kind == sticker_files::Kind::Sticker => {
            let code = format!(":{name}:");
            crate::home::emoji_image(
                ui,
                &picture,
                crate::home::STICKER_SIZE,
                &t!("home.sticker", code = code),
            )
            .on_hover_text(code);
        }
        Some(sticker_files::State::Loading) if kind == sticker_files::Kind::Sticker => {
            ui.allocate_ui(Vec2::splat(64.0), |ui| ui.spinner());
        }
        _ => {}
    }
}

/// A link preview the sender's app made: loaded when on screen, then drawn as a card
/// with the page's picture from its thumbnail.
fn link_card(
    ui: &mut Ui,
    files: &mut Files,
    commands: &UnboundedSender<Command>,
    file: &Attachment,
    links: &[String],
) {
    let area = Rect::from_min_size(ui.cursor().min, Vec2::splat(THUMBNAIL));
    if ui.is_rect_visible(area) {
        bound_cards(&mut files.cards);
        files.cards.entry(file.file.id).or_insert_with(|| {
            let _ = commands.send(Command::LinkCard(file.file.clone()));
            Card::Loading
        });
    }
    let Some(Card::Ready(card)) = files.cards.get(&file.file.id) else {
        return;
    };
    // A sender can't attach a card for a page the message doesn't link to.
    if !link_cards::matches(card, links.iter().map(String::as_str)) {
        return;
    }
    let card = card.clone();
    if let Some(thumb) = &file.thumbnail {
        bound_pictures(
            &mut files.images,
            files.viewer.as_ref().map(|v| v.file.file.id),
        );
        files.images.entry(thumb.id).or_insert_with(|| {
            let _ = commands.send(Command::Image(thumb.clone(), Fetch::Thumbnail));
            Picture::Loading
        });
    }
    let picture = (file.thumbnail.as_ref()).and_then(|thumb| match files.images.get(&thumb.id) {
        Some(Picture::Ready(anim)) => anim.frames.first(),
        _ => None,
    });
    // The address's host always shows, so a card can't pass for another site.
    let host = link_cards::web_url(&card.url)
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_default();
    let kind = if card.site.is_empty() || card.site.eq_ignore_ascii_case(&host) {
        host
    } else {
        format!("{} · {host}", card.site)
    };
    let clicked = crate::home::embed::Embed {
        image: picture.map(crate::home::embed::Image::Wide),
        kind: &kind,
        title: &card.title,
        description: (!card.description.is_empty()).then_some(card.description.as_str()),
        fields: &[],
        buttons: &[crate::home::embed::Button {
            label: t!("previews.open_link"),
            primary: false,
        }],
    }
    .show(ui);
    if clicked.is_some() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(card.url));
    }
}

/// Take the oldest entries off `queue` until the rest hold at most `budget` bytes in
/// total (the newest always stays), and return them.
fn evict(
    queue: &mut std::collections::VecDeque<(Id, usize)>,
    total: &mut usize,
    budget: usize,
) -> Vec<Id> {
    let mut out = Vec::new();
    while *total > budget && queue.len() > 1 {
        if let Some((old, bytes)) = queue.pop_front() {
            *total -= bytes;
            out.push(old);
        }
    }
    out
}

/// Empty the picture cache once it holds too many, except the viewer's (`open`);
/// shown ones load again on demand.
fn bound_pictures(images: &mut HashMap<Id, Picture>, open: Option<Id>) {
    if images.len() > MAX_PICTURES {
        // Loading entries stay so their pending answers still land.
        images.retain(|id, picture| Some(*id) == open || matches!(picture, Picture::Loading));
    }
}

/// The same bound for loaded link previews.
fn bound_cards(cards: &mut HashMap<Id, Card>) {
    if cards.len() > MAX_PICTURES {
        cards.retain(|_, card| matches!(card, Card::Loading));
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

/// A file in a channel's shared files: its name, size and `note` (who added it), with
/// Download, and Remove when `removable`. Returns whether Remove was clicked.
pub fn shared_card(
    ui: &mut Ui,
    files: &mut Files,
    address: &Url,
    file: &Attachment,
    note: &str,
    removable: bool,
) -> bool {
    if files.saves.get(&file.file.id) == Some(&Saving::Gone) {
        gone(ui, file);
        return false;
    }
    let mut remove = false;
    let shown = attachment_frame(ui).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("📄").size(ui::TEXT_ICON_LARGE));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(ui::bold(&file.name));
                ui.label(ui::caption(format!(
                    "{} · {note}",
                    size(file.file.size.into())
                )));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if removable {
                    let size = Vec2::splat(ui::ICON_BUTTON);
                    remove = ui::icon_button(ui, "🗑", t!("files.remove"), ui::Look::Ghost, size)
                        .clicked();
                }
                download(ui, files, address, file);
            });
        });
    });
    if let Some(path) = files.saved_at(file) {
        let card = shown.response.interact(Sense::click());
        card.widget_info(|| WidgetInfo::labeled(Role::Button, true, &file.name));
        saved_menu(&card, path);
    }
    remove
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
    if PICKER && ui::secondary(ui, t!("files.download")).clicked() {
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
            let backdrop = ui.interact(screen, ui.scope_id().with("dismiss"), Sense::click());
            Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(ui::bold(&file.name).color(ON_FILL));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = tool(ui, "×", t!("common.close")).clicked();
                        ui.add_space(12.0);
                        open_it = tool(ui, "↗", t!("files.open_default")).clicked();
                        if PICKER && tool(ui, "⬇", t!("common.save")).clicked() {
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
                            // Match the painted bounds, including rotation and scroll offset.
                            let displayed = Rect::from_center_size(rect.center(), outline * scale);
                            ui.interact(displayed, ui.scope_id().with("image"), Sense::click());
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
            close |= backdrop.clicked();
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

    use egui_kittest::{Harness, kittest::Queryable};

    fn lightbox(dark: bool) -> Harness<'static, Files> {
        lightbox_size(dark, Vec2::new(800.0, 600.0))
    }

    fn lightbox_size(dark: bool, size: Vec2) -> Harness<'static, Files> {
        let mut harness = Harness::builder().with_size(size).build_ui_state(
            |ui, files: &mut Files| {
                let clicked = ui
                    .put(ui.max_rect(), egui::Button::new("Chat image"))
                    .clicked();
                if clicked {
                    assert!(files.viewer.is_none(), "dismissal must not reach chat");
                    let texture = ui.ctx().load_texture(
                        "test image",
                        egui::ColorImage::filled([400, 200], egui::Color32::GREEN),
                        TextureOptions::LINEAR,
                    );
                    files.images.insert(
                        [1; 32],
                        Picture::Ready(Anim {
                            frames: vec![texture],
                            delays: vec![100],
                        }),
                    );
                    files.viewer = Some(Viewer {
                        address: Url::parse("https://localhost/").unwrap(),
                        file: Attachment {
                            file: rusttalk_core::client::BlobRef::for_tests([1; 32]),
                            name: "Test image".into(),
                            media_type: "image/png".into(),
                            thumbnail: None,
                        },
                        commands: tokio::sync::mpsc::unbounded_channel().0,
                        zoom: 1.0,
                        turns: 0,
                    });
                }
                viewer(ui.ctx(), files);
            },
            Files::new(Path::new("/nonexistent/rusttalk-viewer-test")),
        );
        ui::install(&harness.ctx);
        harness.ctx.set_visuals(if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        harness.run_steps(3);
        // Open from a spot which will be dimmed: the opening click must survive.
        pointer(&mut harness, egui::pos2(50.0, 300.0), true, false);
        pointer(&mut harness, egui::pos2(50.0, 300.0), false, false);
        harness.run_steps(3);
        assert!(harness.state().viewer.is_some());
        harness
    }

    fn pointer(harness: &mut Harness<'_, Files>, pos: egui::Pos2, pressed: bool, touch: bool) {
        harness.event(egui::Event::PointerMoved(pos));
        if touch {
            harness.event(egui::Event::Touch {
                device_id: egui::TouchDeviceId(0),
                id: egui::TouchId(0),
                phase: if pressed {
                    egui::TouchPhase::Start
                } else {
                    egui::TouchPhase::End
                },
                pos,
                force: None,
            });
        }
        // Native touch integrations also emit primary pointer events.
        harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(1);
    }

    fn click(harness: &mut Harness<'_, Files>, pos: egui::Pos2, touch: bool) {
        pointer(harness, pos, true, touch);
        pointer(harness, pos, false, touch);
        harness.run_steps(3);
    }

    #[test]
    fn viewer_backdrop_mouse_and_touch_preserve_image_and_toolbar() {
        for dark in [false, true] {
            for touch in [false, true] {
                let mut harness = lightbox(dark);
                click(&mut harness, egui::pos2(400.0, 320.0), touch);
                assert!(harness.state().viewer.is_some(), "image click");
                harness.get_by_label(t!("files.rotate")).click();
                harness.run_steps(3);
                assert_eq!(harness.state().viewer.as_ref().unwrap().turns, 1);
                click(&mut harness, egui::pos2(400.0, 180.0), touch);
                assert!(harness.state().viewer.is_some(), "rotated image click");
                harness.get_by_label(t!("files.zoom_in")).click();
                harness.run_steps(3);
                assert_eq!(harness.state().viewer.as_ref().unwrap().zoom, 1.25);
                harness.get_by_label(t!("files.zoom_out")).click();
                harness.run_steps(3);
                assert_eq!(harness.state().viewer.as_ref().unwrap().zoom, 1.0);
                // This was inside the unrotated image, but is now dimmed.
                click(&mut harness, egui::pos2(230.0, 320.0), touch);
                assert!(harness.state().viewer.is_none(), "backdrop dismissal");
                assert!(!harness.state().images.contains_key(&[1; 32]));
            }
        }
    }

    fn painted_image(harness: &Harness<'_, Files>) -> Rect {
        let Some(Picture::Ready(anim)) = harness.state().images.get(&[1; 32]) else {
            panic!("missing image");
        };
        harness
            .output()
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::epaint::Shape::Mesh(mesh) if mesh.texture_id == anim.frames[0].id() => {
                    Some(mesh.calc_bounds())
                }
                _ => None,
            })
            .expect("painted image")
    }

    #[test]
    fn viewer_dismisses_at_phone_tablet_and_desktop_sizes() {
        for dark in [false, true] {
            for size in [
                Vec2::new(390.0, 844.0),
                Vec2::new(820.0, 1180.0),
                Vec2::new(1100.0, 700.0),
            ] {
                let mut harness = lightbox_size(dark, size);
                let image = painted_image(&harness);
                click(&mut harness, image.center(), true);
                assert!(harness.state().viewer.is_some());
                click(
                    &mut harness,
                    egui::pos2(size.x / 2.0, image.bottom() + 30.0),
                    true,
                );
                assert!(harness.state().viewer.is_none());
            }
        }
    }

    #[test]
    fn viewer_keeps_drag_scroll_escape_and_close() {
        let mut harness = lightbox(true);
        harness.state_mut().viewer.as_mut().unwrap().zoom = 4.0;
        harness.run_steps(3);
        click(&mut harness, egui::pos2(400.0, 320.0), true);
        let before_drag = painted_image(&harness);
        pointer(&mut harness, egui::pos2(400.0, 320.0), true, true);
        harness.event(egui::Event::Touch {
            device_id: egui::TouchDeviceId(0),
            id: egui::TouchId(0),
            phase: egui::TouchPhase::Move,
            pos: egui::pos2(100.0, 200.0),
            force: None,
        });
        harness.event(egui::Event::PointerMoved(egui::pos2(100.0, 200.0)));
        harness.run_steps(3);
        pointer(&mut harness, egui::pos2(100.0, 200.0), false, true);
        let after_drag = painted_image(&harness);
        assert!(
            after_drag.left() < before_drag.left(),
            "drag must pan the image"
        );
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            delta: Vec2::new(-200.0, -200.0),
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(10);
        assert!(
            painted_image(&harness).left() < after_drag.left(),
            "wheel must scroll the image"
        );
        click(&mut harness, egui::pos2(400.0, 320.0), false);
        assert!(harness.state().viewer.is_some(), "panned image");
        harness.get_by_label(t!("common.close")).click();
        harness.run_steps(3);
        assert!(harness.state().viewer.is_none());
        let mut harness = lightbox(false);
        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(3);
        assert!(harness.state().viewer.is_none());
    }

    #[test]
    fn pictures_and_link_previews_stay_bounded() {
        let mut images: HashMap<Id, Picture> = (0..=MAX_PICTURES as u8)
            .map(|i| ([i; 32], Picture::Failed))
            .collect();
        images.insert([254; 32], Picture::Loading);
        bound_pictures(&mut images, Some([3; 32]));
        let mut kept: Vec<_> = images.keys().copied().collect();
        kept.sort();
        assert_eq!(
            kept,
            [[3; 32], [254; 32]],
            "the viewer's and loading ones stay"
        );
        let card = || {
            Card::Ready(link_cards::Card {
                url: "https://example.org/".into(),
                title: "t".into(),
                description: String::new(),
                site: String::new(),
            })
        };
        let mut cards: HashMap<Id, Card> = (0..=MAX_PICTURES as u8)
            .map(|i| ([i; 32], card()))
            .collect();
        cards.insert([254; 32], Card::Loading);
        bound_cards(&mut cards);
        assert_eq!(cards.len(), 1);
    }

    #[test]
    fn received_pictures_are_dropped_oldest_first_by_decoded_bytes() {
        let mut queue: std::collections::VecDeque<(Id, usize)> =
            (1..=5).map(|n| ([n; 32], 100)).collect();
        let mut total = 500;
        // Within the budget nothing goes.
        assert!(evict(&mut queue, &mut total, 500).is_empty());
        let gone = evict(&mut queue, &mut total, 250);
        assert_eq!(gone, [[1; 32], [2; 32], [3; 32]]);
        assert_eq!((queue.len(), total), (2, 200));
        // One picture bigger than the whole budget still stays, as the newest.
        queue.push_back(([6; 32], 900));
        total += 900;
        assert_eq!(evict(&mut queue, &mut total, 250).len(), 2);
        assert_eq!((queue.len(), total), (1, 900));
    }

    #[test]
    fn only_small_files_of_the_preview_type_become_cards() {
        let file = |media_type: &str, size| Attachment {
            file: {
                let mut blob = rusttalk_core::client::BlobRef::for_tests([1; 32]);
                blob.size = size;
                blob
            },
            name: link_cards::FILE_NAME.into(),
            media_type: media_type.into(),
            thumbnail: None,
        };
        assert!(is_card(&file(link_cards::MEDIA_TYPE, 300)));
        // Older apps never test this and show the file; anything else stays a file here too.
        assert!(!is_card(&file(link_cards::MEDIA_TYPE, 5000)));
        assert!(!is_card(&file("application/json", 300)));
    }

    #[test]
    fn staged_previews_follow_the_file_and_stop_when_unstaged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        fs::write(&path, b"one").unwrap();
        let before = preview_key(&path, false);
        fs::write(&path, b"longer").unwrap();
        assert_ne!(
            preview_key(&path, false),
            before,
            "a changed file decodes again"
        );

        let previews = Previews::default();
        let going = Arc::new(AtomicBool::new(true));
        previews
            .lock()
            .insert(preview_key(&path, false), Preview::Loading(going.clone()));
        let staged = Staged {
            path: path.clone(),
            name: "a.png".into(),
            size: 6,
        };
        previews.keep(std::slice::from_ref(&staged));
        assert!(going.load(Ordering::Relaxed));
        previews.keep(&[]);
        assert!(previews.lock().is_empty());
        assert!(!going.load(Ordering::Relaxed), "its decode stops");
    }

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
