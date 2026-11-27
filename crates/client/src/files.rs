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
    self, Align, CornerRadius, Frame, Id as UiId, Image, Layout, Margin, Modal, Rect, Response,
    RichText, Role, ScrollArea, Sense, TextureHandle, TextureOptions, Ui, UiBuilder, Vec2,
    WidgetInfo,
};
use rusttalk_core::client::{Attachment, BlobRef, save_json};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::{
    home::frame_at,
    link_cards,
    net::{Command, Fetch, MAX_FILE, file_name},
    sensitive::{self, Hidden, Sensitive},
    server::{Chat, Id, Picture as Picture2, Staged},
    sticker_files,
    ui::{self, ON_FILL, OVERLAY, OVERLAY_BUTTON, OVERLAY_TEXT},
};

/// Whether this device can pick files to upload: the desktop file dialog, or the
/// phone's own picker (docs/mobile.md).
pub const PICKER: bool = true;
/// Whether files can be saved through a save dialog. Phones hand stored files to the
/// system instead (`export_label`), and can't save live files yet.
pub const SAVE_DIALOG: bool = !cfg!(any(target_os = "android", target_os = "ios"));
/// Whether files can be handed to their app (`open`); phones have no backend for it.
const OPENS: bool = SAVE_DIALOG;

/// Phones: the button that hands a stored file to the system instead of a save
/// dialog (`Files::export`). Android saves it to Downloads, iOS opens the share sheet.
fn export_label() -> Option<&'static str> {
    if cfg!(target_os = "android") {
        Some(t!("files.save_to_phone"))
    } else if cfg!(target_os = "ios") {
        Some(t!("files.share"))
    } else {
        None
    }
}

/// Whether pictures can be copied: egui's phone clipboards take only text.
pub const COPY_IMAGE: bool = !cfg!(any(target_os = "android", target_os = "ios"));

pub(crate) const THUMBNAIL: f32 = 320.0;
/// Animated GIFs and WebPs up to this size load by themselves and play in the chat.
const INLINE_MAX: u64 = 8 * MIB;
/// Pictures without a thumbnail up to this size load their original to show in the chat.
const ORIGINAL_MAX: u64 = 32 * MIB;
/// Animations playing in the chat at once; the ones scrolled away drop first.
const MAX_INLINE: usize = 8;
/// An animation unseen this long (seconds) may be dropped when others need the room.
const INLINE_IDLE: f64 = 1.0;
/// Thumbnails and link previews kept; the ones unseen longest go first and load again
/// when shown.
const MAX_PICTURES: usize = 200;
/// Thumbnails dropped at once when the cache is full, so it is trimmed rarely.
const PICTURES_SLACK: usize = MAX_PICTURES / 4;
/// Decoded bytes all received emoji and sticker files may hold together.
const STICKER_BUDGET: usize = 256 * 1024 * 1024;
/// How far a finger moves sideways in the viewer to go to the next or previous picture.
const SWIPE: f32 = 50.0;

/// Tests answer file dialogs here instead of opening one: given the suggested
/// name when saving (`None` when opening), return the chosen paths.
pub static TEST_ANSWER: Mutex<Option<Answerer>> = Mutex::new(None);
pub type Answerer = fn(Option<&str>) -> Vec<PathBuf>;

/// Tests get the path here instead of the file manager or an app opening:
/// the path, and whether its folder was asked for.
pub static TEST_OPEN: Mutex<Option<Opener>> = Mutex::new(None);
pub type Opener = fn(&Path, bool);

pub struct Files {
    /// Thumbnails, by blob ID.
    images: HashMap<Id, Inline>,
    /// The last picture opened, retained after dismissal so it can reopen offline.
    full: Option<FullPicture>,
    /// Unfinished requests, including for pictures the viewer moved away from.
    full_pending: Vec<(Url, BlobRef)>,
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
    /// Phones: downloads to hand to the system once saved, with their media type
    /// (see `export_label`). The decrypted copy is deleted after.
    export_after: HashMap<Id, String>,
    viewer: Option<Viewer>,
    /// "Jump to message" was picked in the viewer: the gallery's key, author and ID.
    jump: Option<(Id, Id, Id)>,
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
    /// What shows blurred (see `sensitive`).
    pub sensitive: Sensitive,
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
    /// A blurred copy of the first frame, for a sensitive picture.
    blurred: Option<sensitive::Blur>,
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

/// A picture loaded for the chat: a playing animation or a thumbnail.
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

struct FullPicture {
    address: Url,
    blob: BlobRef,
    picture: Picture,
}

impl FullPicture {
    fn matches(&self, address: &Url, blob: &BlobRef) -> bool {
        &self.address == address && &self.blob == blob
    }
}

/// The picture open in the viewer.
struct Viewer {
    address: Url,
    file: Attachment,
    commands: UnboundedSender<Command>,
    zoom: f32,
    /// Quarter turns clockwise.
    turns: u8,
    /// Opened from a gallery: its pictures, newest first, and which one shows.
    album: Option<(Vec<Shot>, usize)>,
    /// Opened in an NSFW channel this person agreed to (see `sensitive::agreed_nsfw`).
    nsfw: bool,
}

/// A picture in a gallery the viewer can step through.
#[derive(Clone)]
pub struct Shot {
    pub address: Url,
    pub commands: UnboundedSender<Command>,
    pub file: Attachment,
    /// Who shared it and when.
    pub caption: String,
    /// The gallery's key with the message's author and ID, when the chat can jump there.
    pub jump: Option<(Id, Id, Id)>,
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
    /// Pictures for new server emoji or (`true`) stickers, to check before uploading.
    Emoji(Url, bool),
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
            full: None,
            full_pending: Vec::new(),
            inline: HashMap::new(),
            saves: HashMap::new(),
            paths,
            data: data.to_owned(),
            open_after: HashSet::new(),
            export_after: HashMap::new(),
            viewer: None,
            jump: None,
            asking: false,
            answers: None,
            pastes: mpsc::channel(),
            cards: HashMap::new(),
            stickers: Default::default(),
            sticker_bytes: 0,
            sensitive: Sensitive::new(data),
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
        address: &Url,
        blob: &BlobRef,
        fetch: Fetch,
        frames: Result<Vec<(egui::ColorImage, u32)>, bool>,
    ) {
        if fetch == Fetch::Copy {
            if let Some((image, _)) = frames.ok().and_then(|frames| frames.into_iter().next()) {
                ctx.copy_image(image);
            }
            return;
        }
        let slot = match fetch {
            Fetch::Inline => self
                .inline
                .get_mut(&blob.id)
                .map(|inline| &mut inline.picture),
            Fetch::Thumbnail => self
                .images
                .get_mut(&blob.id)
                .map(|thumb| &mut thumb.picture),
            Fetch::Full => {
                self.full_pending
                    .retain(|(source, file)| source != address || file != blob);
                self.full
                    .as_mut()
                    .filter(|full| {
                        full.matches(address, blob) && matches!(full.picture, Picture::Loading)
                    })
                    .map(|full| &mut full.picture)
            }
            Fetch::Copy => None,
        };
        let Some(slot) = slot else {
            return;
        };
        let mut first = None;
        *slot = match frames {
            Ok(frames) => {
                let delays = frames.iter().map(|(_, ms)| *ms).collect();
                // A whole picture is blurred from its thumbnail's copy (`blurred_of`).
                let blurred = (frames.first())
                    .filter(|_| fetch != Fetch::Full)
                    .map(|(image, _)| sensitive::blurred(image));
                // A whole picture opens from a thumbnail or copy that was checked already.
                if fetch != Fetch::Full && self.sensitive.wants(&blob.id) {
                    first = frames.first().map(|(image, _)| image.clone());
                }
                let frames = (frames.into_iter())
                    .map(|(image, _)| ctx.load_texture("file", image, TextureOptions::LINEAR))
                    .collect();
                Picture::Ready(Anim {
                    frames,
                    delays,
                    blurred,
                })
            }
            Err(true) => Picture::Gone,
            Err(false) => Picture::Failed,
        };
        // Checked only now, as it is about to show.
        if let Some(image) = first {
            self.sensitive.check(ctx, blob.id, &image);
        }
    }

    fn open_viewer(
        &mut self,
        address: &Url,
        commands: &UnboundedSender<Command>,
        file: &Attachment,
    ) {
        let reuse = self.full.as_ref().is_some_and(|full| {
            full.matches(address, &file.file)
                && matches!(full.picture, Picture::Loading | Picture::Ready(_))
        });
        if !reuse {
            let request = (address.clone(), file.file.clone());
            let picture = if self.full_pending.contains(&request) {
                Picture::Loading
            } else if commands
                .send(Command::Image(file.file.clone(), Fetch::Full))
                .is_ok()
            {
                self.full_pending.push(request);
                Picture::Loading
            } else {
                Picture::Failed
            };
            self.full = Some(FullPicture {
                address: address.clone(),
                blob: file.file.clone(),
                picture,
            });
        }
        self.viewer = Some(Viewer {
            address: address.clone(),
            file: file.clone(),
            commands: commands.clone(),
            zoom: 1.0,
            turns: 0,
            album: None,
            nsfw: self.sensitive.nsfw_here,
        });
    }

    /// The blurred copy made from a file's thumbnail or small copy, once loaded.
    fn blurred_of(&self, file: &Attachment) -> Option<&sensitive::Blur> {
        let thumb = file.thumbnail.as_ref().and_then(|t| self.images.get(&t.id));
        [thumb, self.inline.get(&file.file.id)]
            .into_iter()
            .flatten()
            .find_map(|loaded| match &loaded.picture {
                Picture::Ready(anim) => anim.blurred.as_ref(),
                _ => None,
            })
    }

    /// Open the viewer on a gallery's picture `at`, with next and previous.
    pub fn open_album(&mut self, album: Vec<Shot>, at: usize) {
        let Some(shot) = album.get(at) else {
            return;
        };
        let nsfw = (self.viewer.as_ref()).map_or(self.sensitive.nsfw_here, |view| view.nsfw);
        let (address, commands, file) = (
            shot.address.clone(),
            shot.commands.clone(),
            shot.file.clone(),
        );
        self.open_viewer(&address, &commands, &file);
        if let Some(view) = &mut self.viewer {
            view.album = Some((album, at));
            // Stepping keeps where the gallery was opened.
            view.nsfw = nsfw;
        }
    }

    /// The message "Jump to message" in the viewer asked for: author and ID, if it was
    /// for the gallery `key`.
    pub fn take_jump(&mut self, key: Id) -> Option<(Id, Id)> {
        let (_, person, id) = self.jump.filter(|jump| jump.0 == key)?;
        self.jump = None;
        Some((person, id))
    }

    /// Hand a file the app can't show to its app (desktop) or the system (phones).
    pub fn open_elsewhere(
        &mut self,
        ctx: &egui::Context,
        (address, commands): (&Url, &UnboundedSender<Command>),
        file: &Attachment,
    ) {
        if OPENS {
            self.open_file(commands, file);
        } else if export_label().is_some() {
            self.export(ctx, address, file);
        } else if SAVE_DIALOG {
            self.ask(ctx, Ask::Save(address.clone(), file.clone()));
        }
    }

    /// How far a download is (0 to 1), while it runs.
    fn busy(&self, id: &Id) -> Option<f32> {
        match self.saves.get(id) {
            Some(Saving::Busy(done, _)) => Some(*done),
            _ => None,
        }
    }

    pub fn forget_full_pictures(&mut self, address: &Url) {
        self.full_pending.retain(|(source, _)| source != address);
        if self
            .full
            .as_ref()
            .is_some_and(|full| &full.address == address)
        {
            self.full = None;
        }
        if self
            .viewer
            .as_ref()
            .is_some_and(|view| &view.address == address)
        {
            self.viewer = None;
        }
    }

    /// Start loading the playing copy of an animated file once it is on screen, after
    /// dropping the ones scrolled away if too many are kept.
    fn want_inline(&mut self, ui: &Ui, commands: &UnboundedSender<Command>, file: &Attachment) {
        let area = Rect::from_min_size(ui.cursor().min, Vec2::splat(THUMBNAIL));
        if !ui.is_rect_visible(area) {
            return;
        }
        self.want_inline_at(ui.input(|input| input.time), commands, file);
    }

    /// Start loading the playing copy of an animated file that is on screen at `now`.
    fn want_inline_at(&mut self, now: f64, commands: &UnboundedSender<Command>, file: &Attachment) {
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

    /// A download finished. Returns a notice to show (phones, see `export_label`).
    pub fn saved(&mut self, id: Id, saved: Result<(), bool>) -> Option<String> {
        let path = match self.saves.remove(&id) {
            Some(Saving::Busy(_, path)) => Some(path),
            _ => None,
        };
        if let Some(media_type) = self.export_after.remove(&id) {
            let (state, notice) = match (saved, path) {
                (Ok(()), Some(path)) => hand_over(&path, &media_type),
                (saved, path) => {
                    if let Some(path) = path {
                        discard(&path);
                    }
                    match saved {
                        Err(true) => (Some(Saving::Gone), None),
                        _ => (
                            Some(Saving::Failed),
                            Some(t!("files.export_failed").to_owned()),
                        ),
                    }
                }
            };
            if let Some(state) = state {
                self.saves.insert(id, state);
            }
            return notice;
        }
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
        None
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
                Ask::Attach(..)
                    | Ask::AttachDm(_)
                    | Ask::PagePictures(..)
                    | Ask::Share(..)
                    | Ask::Emoji(..)
            );
            let pictures = matches!(
                ask,
                Ask::Picture | Ask::Icon(_) | Ask::Emoji(..) | Ask::PagePictures(..)
            );
            let zip = matches!(ask, Ask::EmojiZip(_));
            let test = *TEST_ANSWER.lock().unwrap_or_else(|e| e.into_inner());
            let paths = match (test, name) {
                (Some(answer), name) => answer(name.as_deref()),
                // Phones save through `export_label`; their save buttons are off.
                #[cfg(target_os = "android")]
                (None, Some(_)) => {
                    tracing::warn!("no save dialog on this device");
                    Vec::new()
                }
                #[cfg(target_os = "android")]
                (None, None) => {
                    use crate::android::Pick;
                    let kind = if pictures {
                        Pick::Pictures
                    } else if zip {
                        Pick::Zip
                    } else {
                        Pick::Any
                    };
                    crate::android::pick(kind, !single)
                }
                // The save buttons are off (`SAVE_DIALOG`); stored files are shared.
                #[cfg(target_os = "ios")]
                (None, Some(_)) => {
                    tracing::warn!("no save dialog on this device");
                    Vec::new()
                }
                #[cfg(target_os = "ios")]
                (None, None) => {
                    use crate::ios::Pick;
                    let what = match (pictures, zip) {
                        (true, _) => Pick::Pictures,
                        (false, true) => Pick::Zip,
                        (false, false) => Pick::Any,
                    };
                    crate::ios::pick(what, !single)
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

    /// Phones: decrypt a stored file into a temporary folder, as a save dialog's
    /// answer, then hand it to the system once saved (see `saved`).
    fn export(&mut self, ctx: &egui::Context, address: &Url, file: &Attachment) {
        if matches!(self.saves.get(&file.file.id), Some(Saving::Busy(..))) {
            return;
        }
        let folder = temp_folder(&file.file.id[..8]);
        if let Err(error) = fs::create_dir_all(&folder) {
            tracing::warn!(%error, "making a folder to save a file failed");
            self.saves.insert(file.file.id, Saving::Failed);
            return;
        }
        let media_type = match file.media_type.as_str() {
            "" => "application/octet-stream",
            known => known,
        };
        (self.export_after).insert(file.file.id, media_type.to_owned());
        let path = folder.join(file.name.replace(['/', '\\'], "_"));
        let answers = self.answers.get_or_insert_with(mpsc::channel).0.clone();
        let _ = answers.send((Ask::Save(address.clone(), file.clone()), vec![path]));
        ctx.request_repaint();
    }
}

/// Hand a decrypted file to the system: the new state and a notice to show. Its
/// temporary folder is deleted once the system has its copy.
#[cfg(target_os = "android")]
fn hand_over(path: &Path, media_type: &str) -> (Option<Saving>, Option<String>) {
    let shared = crate::android::save_to_downloads(path, media_type);
    discard(path);
    match shared {
        Ok(true) => (
            Some(Saving::Done),
            Some(t!("files.saved_to_downloads").to_owned()),
        ),
        Ok(false) => (
            Some(Saving::Done),
            Some(t!("files.saved_to_app_folder").to_owned()),
        ),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "saving a file to Downloads failed");
            (
                Some(Saving::Failed),
                Some(t!("files.export_failed").to_owned()),
            )
        }
    }
}

/// The share sheet deletes the folder when it closes.
#[cfg(target_os = "ios")]
fn hand_over(path: &Path, _media_type: &str) -> (Option<Saving>, Option<String>) {
    if crate::ios::share(path) {
        return (None, None);
    }
    discard(path);
    (
        Some(Saving::Failed),
        Some(t!("files.share_failed").to_owned()),
    )
}

/// Desktops never export: they have a save dialog.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn hand_over(path: &Path, _media_type: &str) -> (Option<Saving>, Option<String>) {
    discard(path);
    (Some(Saving::Failed), None)
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

/// Copy a file a phone's picker handed over (and may delete) into a fresh
/// `rusttalk-<id>` temporary folder under `name`, as a pasted picture: `discard` and
/// `clear_opened` remove it later.
#[cfg_attr(not(target_os = "ios"), allow(dead_code))]
pub fn keep_picked(from: &Path, name: &str) -> io::Result<PathBuf> {
    let name = match name.replace(['/', '\\'], "_").trim() {
        "" | "." | ".." => "file".to_owned(),
        name => name.to_owned(),
    };
    let id = rusttalk_core::keys::random_key().map_err(io::Error::other)?;
    let folder = temp_folder(&id[..8]);
    fs::create_dir(&folder)?;
    let path = folder.join(name);
    if let Err(error) = fs::copy(from, &path) {
        let _ = fs::remove_dir_all(&folder);
        return Err(error);
    }
    Ok(path)
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

/// A picture drawn in a message, for its menu.
pub struct Shown {
    pub response: Response,
    pub media: Media,
}

/// A stored picture a menu acts on: where it is stored, the file, and where this
/// device saved it.
#[derive(Clone)]
pub struct Media {
    address: Url,
    commands: UnboundedSender<Command>,
    file: Attachment,
    saved: Option<PathBuf>,
    /// Shows blurred now, so its menu offers to show it instead of blurring it.
    blurred: bool,
}

/// What a picture's menu asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MediaAction {
    Copy,
    SaveAs,
    View,
    ShowInFolder,
    Open,
    /// Blur (`true`) or show it from now on, on this device.
    Blur(bool),
}

/// A picture's own items, above the message's in its menu.
pub fn media_items(m: &mut ui::Menu, media: &Media) -> Option<MediaAction> {
    let mut action = copy_save_items(m);
    if m.item("🔍", t!("files.open_viewer")).clicked() {
        action = Some(MediaAction::View);
    }
    if media.saved.is_some() && m.item("🗁", t!("files.show_in_folder")).clicked() {
        action = Some(MediaAction::ShowInFolder);
    }
    if m.item("↗", t!("files.open_default")).clicked() {
        action = Some(MediaAction::Open);
    }
    blur_item(m, media).or(action)
}

/// "Blur" or "Show" for this device, in a picture's or video's menu.
fn blur_item(m: &mut ui::Menu, media: &Media) -> Option<MediaAction> {
    let (icon, label) = if media.blurred {
        ("👁", t!("sensitive.unblur"))
    } else {
        ("🙈", t!("sensitive.blur"))
    };
    m.item(icon, label)
        .clicked()
        .then_some(MediaAction::Blur(!media.blurred))
}

/// "Copy image" and "Save as…", where the device can: a picture's menu in the chat and
/// in the viewer.
fn copy_save_items(m: &mut ui::Menu) -> Option<MediaAction> {
    let mut action = None;
    if COPY_IMAGE && m.item("🖼", t!("files.copy_image")).clicked() {
        action = Some(MediaAction::Copy);
    }
    if SAVE_DIALOG && m.item("⬇", t!("files.save_as")).clicked() {
        action = Some(MediaAction::SaveAs);
    }
    action
}

impl Files {
    pub fn media_action(&mut self, ctx: &egui::Context, action: MediaAction, media: &Media) {
        let Media {
            address,
            commands,
            file,
            saved,
            ..
        } = media;
        match action {
            // Another server's sticker is decoded already.
            MediaAction::Copy => match sticker_files::picture(ctx, &file.file.id) {
                Some(sticker) => ctx.copy_image(sticker.first.0.clone()),
                None => {
                    let _ = commands.send(Command::Image(file.file.clone(), Fetch::Copy));
                }
            },
            MediaAction::SaveAs => self.ask(ctx, Ask::Save(address.clone(), file.clone())),
            MediaAction::View => self.open_viewer(address, commands, file),
            MediaAction::ShowInFolder => {
                if let Some(path) = saved {
                    open(path, true);
                }
            }
            MediaAction::Open => self.open_file(commands, file),
            MediaAction::Blur(blur) => self.sensitive.choose(file, blur),
        }
    }
}

/// Menus with only the pictures' own items, outside chat messages.
fn media_menus(files: &mut Files, shown: Vec<Shown>) {
    for Shown { response, media } in shown {
        let action = ui::context_menu(&response, |m| media_items(m, &media)).flatten();
        if let Some(action) = action {
            files.media_action(&response.ctx, action, &media);
        }
    }
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

/// Add picked or dropped files to the composer, up to `max` (the server's stored files
/// per message); returns a problem to show.
pub fn stage(chat: &mut Chat, paths: Vec<PathBuf>, max: usize) -> Option<String> {
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
        } else if chat.staged.len() >= max {
            problem = Some(t!("files.too_many", max = max));
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
            *entry = Preview::Ready(Anim {
                frames,
                delays,
                blurred: None,
            });
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

/// The staged files in the composer, as equal square tiles in a row that scrolls
/// sideways: pictures show themselves, play on hover and open large on click; other
/// files, and pictures that can't be shown, show an icon with their name and size.
/// Each has a remove button in its corner. Under them, "Mark as sensitive" when there
/// are pictures or videos.
pub fn staged(ui: &mut Ui, chat: &mut Chat) {
    ui.vertical(|ui| {
        staged_row(ui, chat);
        sensitive_mark(ui, chat);
    });
}

fn staged_row(ui: &mut Ui, chat: &mut Chat) {
    let previews = Previews::get(ui.ctx());
    let open_id = UiId::unique("staged-open");
    previews.keep(&chat.staged);
    let mut remove = None;
    let mut open = None;
    ScrollArea::horizontal()
        .id_salt("staged-files")
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (at, file) in chat.staged.iter().enumerate() {
                    let (shown, removed) = if is_picture(&file.name) {
                        previews.with(ui, &file.path, false, |ui, anim, failed| {
                            let look = match anim {
                                Some(anim) => Tile::Picture(anim),
                                None if failed => Tile::File,
                                None => Tile::Loading,
                            };
                            staged_tile(ui, file, look)
                        })
                    } else {
                        staged_tile(ui, file, Tile::File)
                    };
                    open = open.or(shown.then_some(at));
                    remove = remove.or(removed.then_some(at));
                }
            });
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

/// The mark goes on the message's pictures and videos (see `sensitive`).
fn sensitive_mark(ui: &mut Ui, chat: &mut Chat) {
    let media = (chat.staged.iter()).any(|file| {
        let name = file.name.to_ascii_lowercase();
        let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
        is_picture(&name) || VIDEOS.contains(&extension) || OTHER_PICTURES.contains(&extension)
    });
    if media {
        ui::checkbox(ui, &mut chat.mark_sensitive, t!("sensitive.mark"))
            .on_hover_text(t!("sensitive.mark_help"));
    } else {
        chat.mark_sensitive = false;
    }
}

/// What a staged file's tile shows.
enum Tile<'a> {
    Picture(&'a Anim),
    Loading,
    /// An icon with the name and size: not a picture, or one that can't be shown.
    File,
}

/// A staged file's square tile with a × in its corner. A picture is cropped to the
/// square and plays while hovered. Returns whether a picture was clicked and whether
/// × was.
fn staged_tile(ui: &mut Ui, file: &Staged, look: Tile) -> (bool, bool) {
    let picture = matches!(look, Tile::Picture(_));
    let sense = if picture {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(PREVIEW), sense);
    let c = ui::colors(ui);
    ui.painter().rect(
        rect,
        ui::RADIUS,
        c.raised,
        egui::Stroke::new(1.0, c.border),
        egui::StrokeKind::Inside,
    );
    match look {
        Tile::Picture(anim) => {
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
        Tile::Loading => {
            ui.put(rect, egui::Spinner::new());
        }
        Tile::File => {
            let inner = rect.shrink(ui::SPACE_S);
            let layout = Layout::bottom_up(Align::Min);
            ui.scope_builder(UiBuilder::new().max_rect(inner).layout(layout), |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.add(egui::Label::new(ui::caption(size(file.size))).selectable(false));
                ui.add(egui::Label::new(&file.name).truncate().selectable(false));
                ui.add_space(ui::SPACE_XS);
                ui.label(RichText::new("📄").size(ui::TEXT_ICON_LARGE));
            });
        }
    }
    ui::focus_ring(ui, &response, ui::RADIUS);
    let label = format!("{} · {}", file.name, size(file.size));
    let role = if picture { Role::Button } else { Role::Image };
    response.widget_info(|| WidgetInfo::labeled(role, true, &label));
    let response = response.on_hover_text(&label);
    let shown = if picture {
        response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
    } else {
        false
    };
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

/// Whether a file is a small animation that plays by itself where it shows.
pub fn animates(file: &Attachment) -> bool {
    plays_inline(&file.name, &file.media_type, file.file.size.into())
}

/// Whether a picture without a thumbnail loads its original to show in the chat:
/// older apps sent large phone photos without one.
fn shows_original(file: &Attachment) -> bool {
    file.thumbnail.is_none() && is_picture(&file.name) && u64::from(file.file.size) <= ORIGINAL_MAX
}

/// What a file is in a channel's gallery.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MediaKind {
    /// A picture the viewer can show: it has a thumbnail or the app decodes its type.
    Picture,
    /// A video. The app doesn't play videos: it opens them in their own app.
    Video,
    /// Another kind of picture the app can't decode yet, such as SVG or HEIC.
    Other,
}

const VIDEOS: [&str; 8] = ["mp4", "m4v", "webm", "mov", "mkv", "avi", "ogv", "3gp"];
const OTHER_PICTURES: [&str; 8] = ["svg", "heic", "heif", "avif", "bmp", "tif", "tiff", "ico"];

/// Whether a message's file goes in the gallery, and as what: by its thumbnail, or else
/// its name and type, which only the sender's app chose.
pub fn media_kind(file: &Attachment) -> Option<MediaKind> {
    if is_card(file) || sticker_files::of(file).is_some() {
        return None;
    }
    let name = file.name.to_ascii_lowercase();
    let extension = name.rsplit_once('.').map_or("", |(_, ext)| ext);
    let kind = file.media_type.split('/').next().unwrap_or_default();
    if file.thumbnail.is_some() || is_picture(&name) {
        Some(MediaKind::Picture)
    } else if kind == "video" || VIDEOS.contains(&extension) {
        Some(MediaKind::Video)
    } else if kind == "image" || OTHER_PICTURES.contains(&extension) {
        Some(MediaKind::Other)
    } else {
        None
    }
}

/// A picture or video cut to fill `rect`: its thumbnail (or, without one, its original
/// decoded small), or its small animation playing when `play`, with a badge for a GIF,
/// a video or another type and the download's progress. Shared by the gallery's tiles
/// and a message's album.
#[allow(clippy::too_many_arguments)]
fn media_face(
    ui: &mut Ui,
    files: &mut Files,
    commands: &UnboundedSender<Command>,
    file: &Attachment,
    kind: MediaKind,
    rect: Rect,
    play: bool,
    radius: CornerRadius,
) -> Option<Hidden> {
    let c = ui::colors(ui);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, radius, c.side);
    let now = ui.input(|input| input.time);
    files.sensitive.poll();
    let hidden = files.sensitive.hidden(file);
    let play = play
        && hidden.is_none()
        && plays_inline(&file.name, &file.media_type, file.file.size.into());
    // A picture sent without a thumbnail shows its original, decoded small, as in the
    // chat; only while its cell is on screen.
    let original = kind == MediaKind::Picture && shows_original(file);
    if play || (original && ui.is_rect_visible(rect)) {
        files.want_inline_at(now, commands, file);
    }
    let loaded = match files.inline.get(&file.file.id) {
        Some(Inline {
            picture: Picture::Ready(anim),
            ..
        }) if play || original => Some(anim),
        _ => None,
    };
    let playing = loaded.filter(|_| play).map(|anim| anim.current(ui, true));
    if playing.is_none()
        && let Some(thumb) = &file.thumbnail
    {
        want_thumbnail(&mut files.images, now, commands, thumb);
    }
    let thumb = (file.thumbnail.as_ref())
        .and_then(|thumb| files.images.get(&thumb.id))
        .and_then(|thumb| match &thumb.picture {
            Picture::Ready(anim) => Some(anim),
            _ => None,
        });
    let still = (thumb.and_then(|anim| anim.frames.first()))
        .or_else(|| loaded.filter(|_| playing.is_none())?.frames.first());
    let shown = playing.or(still);
    if let Some(hidden) = hidden {
        let blurred = thumb.or(loaded).and_then(|anim| anim.blurred.as_ref());
        paint_blurred(ui, blurred, rect, radius, hidden);
    } else if let Some(texture) = shown {
        let uv = fill_uv(texture.size_vec2(), rect.size());
        Image::new(texture)
            .uv(uv)
            .corner_radius(radius)
            .paint_at(ui, rect);
    }
    match kind {
        _ if hidden.is_some() => {}
        MediaKind::Video => {
            let center = rect.center();
            let radius = (rect.width() * 0.16).clamp(12.0, 22.0);
            painter.circle_filled(center, radius, OVERLAY_BUTTON);
            painter.text(
                center + Vec2::new(radius * 0.12, 0.0),
                egui::Align2::CENTER_CENTER,
                "▶",
                egui::FontId::proportional(radius),
                ON_FILL,
            );
        }
        _ if shown.is_none() => {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "🖼",
                egui::FontId::proportional(ui::TEXT_ICON_LARGE),
                c.muted,
            );
        }
        _ => {}
    }
    // What it is when the picture doesn't show it: GIF until it plays, or the type.
    let tag = match kind {
        MediaKind::Picture if shown.is_some() => animated_kind(&file.name, &file.media_type)
            .filter(|_| playing.is_none())
            .map(str::to_owned),
        _ => (file.name.rsplit_once('.'))
            .map(|(_, ext)| ext.to_ascii_uppercase())
            .filter(|ext| (1..=5).contains(&ext.len())),
    };
    if let Some(tag) = tag {
        let corner = rect.left_bottom() + Vec2::new(4.0, -20.0);
        let corner = Rect::from_min_size(corner, Vec2::new(rect.width() - 8.0, 18.0));
        ui.scope_builder(UiBuilder::new().max_rect(corner), |ui| {
            ui::badge(ui, &tag, OVERLAY_BUTTON);
        });
    }
    if let Some(done) = files.busy(&file.file.id) {
        let bar = Rect::from_min_max(
            rect.left_bottom() - Vec2::new(0.0, 4.0),
            rect.right_bottom(),
        );
        painter.rect_filled(bar, 0.0, OVERLAY_BUTTON);
        let fill = Rect::from_min_size(bar.min, Vec2::new(bar.width() * done, bar.height()));
        painter.rect_filled(fill, 0.0, ui::accent(ui).base);
    }
    hidden
}

/// A sensitive picture in `rect`: its blurred copy (or a plain fill before it loads),
/// with an eye and "Sensitive" over it, unless it is only being checked.
fn paint_blurred(
    ui: &Ui,
    blurred: Option<&sensitive::Blur>,
    rect: Rect,
    radius: CornerRadius,
    hidden: Hidden,
) {
    let painter = ui.painter_at(rect);
    match blurred.map(|blur| blur.texture(ui.ctx())) {
        Some(texture) => Image::new(texture)
            .uv(fill_uv(texture.size_vec2(), rect.size()))
            .corner_radius(radius)
            .paint_at(ui, rect),
        None => {
            painter.rect_filled(rect, radius, ui::colors(ui).side);
        }
    }
    if hidden == Hidden::Checking {
        return;
    }
    painter.rect_filled(rect, radius, OVERLAY_BUTTON);
    let roomy = rect.width() >= 96.0 && rect.height() >= 72.0;
    let eye = rect.center() - Vec2::new(0.0, if roomy { 10.0 } else { 0.0 });
    let font = egui::FontId::proportional(ui::TEXT_ICON_LARGE);
    painter.text(eye, egui::Align2::CENTER_CENTER, "👁", font, ON_FILL);
    if roomy {
        painter.text(
            rect.center() + Vec2::new(0.0, 14.0),
            egui::Align2::CENTER_CENTER,
            t!("sensitive.label"),
            egui::FontId::proportional(ui::TEXT_SMALL),
            ON_FILL,
        );
    }
}

/// What a screen reader says for a picture, blurred or not.
fn media_label(file: &Attachment, hidden: Option<Hidden>) -> String {
    match hidden {
        Some(Hidden::Sensitive) => format!("{} · {}", file.name, t!("sensitive.tap_to_show")),
        _ => file.name.clone(),
    }
}

/// A square tile in a gallery: the thumbnail cut to a square (playing when `play`, for
/// a small animation), with a badge for a GIF, a video or another type. Clicking it is
/// the caller's; this adds the file's menu, with "Jump to message" when the shot has
/// one. Returns the tile and whether "Jump to message" was picked.
pub fn gallery_tile(
    ui: &mut Ui,
    files: &mut Files,
    shot: &Shot,
    kind: MediaKind,
    rect: Rect,
    play: bool,
) -> (bool, bool) {
    let file = &shot.file;
    let commands = &shot.commands;
    let response = ui.interact(
        rect,
        UiId::unique(("gallery-tile", file.file.id)),
        Sense::click(),
    );
    let hidden = media_face(
        ui,
        files,
        commands,
        file,
        kind,
        rect,
        play,
        CornerRadius::ZERO,
    );
    let label = format!("{} · {}", media_label(file, hidden), shot.caption);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
    let painter = ui.painter_at(rect);
    if response.hovered() {
        painter.rect_filled(rect, 0.0, egui::Color32::from_white_alpha(20));
    }
    let response = response
        .on_hover_text(&label)
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let media = Media {
        address: shot.address.clone(),
        commands: commands.clone(),
        file: file.clone(),
        saved: files.saved_at(file).cloned(),
        blurred: hidden == Some(Hidden::Sensitive),
    };
    let picked = ui::context_menu(&response, |m| {
        let action = if kind == MediaKind::Picture {
            media_items(m, &media)
        } else {
            let open =
                (m.item("↗", t!("files.open_default")).clicked()).then_some(MediaAction::Open);
            blur_item(m, &media).or(open)
        };
        let jump = shot.jump.is_some() && m.item("💬", t!("gallery.jump")).clicked();
        (action, jump)
    });
    let (action, jump) = picked.unwrap_or_default();
    match action {
        Some(MediaAction::Open) if kind != MediaKind::Picture => {
            files.open_elsewhere(ui.ctx(), (&shot.address, commands), file);
        }
        Some(action) => files.media_action(ui.ctx(), action, &media),
        None => {}
    }
    let open = response.clicked() && !reveal_tap(files, file, hidden);
    (open, jump)
}

/// A tap on a blurred picture shows it instead of opening it; whether it did.
fn reveal_tap(files: &mut Files, file: &Attachment, hidden: Option<Hidden>) -> bool {
    let tap = hidden == Some(Hidden::Sensitive);
    if tap {
        files.sensitive.reveal(file);
    }
    tap
}

/// An album is at most this wide, and as wide as the chat on a phone.
const ALBUM_WIDTH: f32 = 400.0;
const ALBUM_GAP: f32 = 2.0;
/// Cells an album shows; the last one tells how many more there are.
const ALBUM_CELLS: usize = 4;

/// Where an album of `count` (2 or more) pictures `width` wide puts them, from its top
/// left, and how tall it is: two side by side, three as a big one and two small ones,
/// and four or more as two by two. Every cell is square.
pub(crate) fn album_cells(count: usize, width: f32) -> (Vec<Rect>, f32) {
    let cell =
        |x: f32, y: f32, w: f32, h: f32| Rect::from_min_size(egui::pos2(x, y), Vec2::new(w, h));
    let half = (width - ALBUM_GAP) / 2.0;
    match count {
        0..=2 => (
            vec![
                cell(0.0, 0.0, half, half),
                cell(half + ALBUM_GAP, 0.0, half, half),
            ],
            half,
        ),
        3 => {
            let small = (width - 2.0 * ALBUM_GAP) / 3.0;
            let big = 2.0 * small + ALBUM_GAP;
            let x = big + ALBUM_GAP;
            let cells = vec![
                cell(0.0, 0.0, big, big),
                cell(x, 0.0, small, small),
                cell(x, small + ALBUM_GAP, small, small),
            ];
            (cells, big)
        }
        _ => {
            let far = half + ALBUM_GAP;
            let cells = vec![
                cell(0.0, 0.0, half, half),
                cell(far, 0.0, half, half),
                cell(0.0, far, half, half),
                cell(far, far, half, half),
            ];
            (cells, width)
        }
    }
}

/// Rounded corners for the corners of `cell` that are the album's (`outer`) own.
fn outer_corners(cell: Rect, outer: Rect) -> CornerRadius {
    let at = |a: f32, b: f32| (a - b).abs() < 0.5;
    let (left, top) = (at(cell.left(), outer.left()), at(cell.top(), outer.top()));
    let (right, bottom) = (
        at(cell.right(), outer.right()),
        at(cell.bottom(), outer.bottom()),
    );
    let round = |yes: bool| if yes { ui::RADIUS } else { 0 };
    CornerRadius {
        nw: round(left && top),
        ne: round(right && top),
        sw: round(left && bottom),
        se: round(right && bottom),
    }
}

/// A message's pictures and videos as one tile with a grid inside. Thumbnails load only
/// while it is on screen. A picture opens the viewer, which steps through the message's
/// pictures; a video opens in its own app. Returns the pictures, for their menus.
fn album(
    ui: &mut Ui,
    files: &mut Files,
    (address, commands): (&Url, &UnboundedSender<Command>),
    media: &[(&Attachment, MediaKind)],
) -> Vec<Shown> {
    let mut shown = Vec::new();
    let width = ui.available_width().min(ALBUM_WIDTH);
    let (cells, height) = album_cells(media.len(), width);
    let (outer, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    if !ui.is_rect_visible(outer) {
        return shown;
    }
    let more = media.len().saturating_sub(ALBUM_CELLS - 1);
    let mut open = None;
    for (n, (cell, (file, kind))) in cells.iter().zip(media).enumerate() {
        let rect = cell.translate(outer.min.to_vec2());
        let radius = outer_corners(rect, outer);
        let response = ui.interact(
            rect,
            ui.scope_id().with(("album", n, file.file.id)),
            Sense::click(),
        );
        let hidden = media_face(ui, files, commands, file, *kind, rect, true, radius);
        let painter = ui.painter_at(rect);
        let last = n == ALBUM_CELLS - 1 && media.len() > ALBUM_CELLS;
        let name = media_label(file, hidden);
        let label = if last {
            format!("{name} · {}", t!("files.album_more", count = more))
        } else {
            name
        };
        if last {
            painter.rect_filled(rect, radius, OVERLAY_BUTTON);
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                format!("+{more}"),
                egui::FontId::proportional(ui::TEXT_HEADING),
                ON_FILL,
            );
        }
        if response.hovered() {
            painter.rect_filled(rect, radius, egui::Color32::from_white_alpha(20));
        }
        response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
        let response = response
            .on_hover_text(&label)
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if response.clicked() && !reveal_tap(files, file, hidden) {
            open = Some(n);
        }
        if *kind == MediaKind::Picture {
            shown.push(Shown {
                response,
                media: Media {
                    address: address.clone(),
                    commands: commands.clone(),
                    file: (*file).clone(),
                    saved: files.saved_at(file).cloned(),
                    blurred: hidden == Some(Hidden::Sensitive),
                },
            });
        }
    }
    if let Some(n) = open {
        let (file, kind) = media[n];
        if kind != MediaKind::Picture {
            files.open_elsewhere(ui.ctx(), (address, commands), file);
            return shown;
        }
        let pictures: Vec<&Attachment> = (media.iter())
            .filter(|(_, kind)| *kind == MediaKind::Picture)
            .map(|(file, _)| *file)
            .collect();
        let at = pictures.iter().position(|p| p.file.id == file.file.id);
        let album = (pictures.into_iter())
            .map(|file| Shot {
                address: address.clone(),
                commands: commands.clone(),
                file: file.clone(),
                // In the chat the message itself says who shared it and when.
                caption: String::new(),
                jump: None,
            })
            .collect();
        files.open_album(album, at.unwrap_or_default());
    }
    shown
}

/// The middle of a picture of `size` that fills a cell shaped like `cell`, in texture
/// coordinates.
fn fill_uv(size: Vec2, cell: Vec2) -> Rect {
    let size = size.max(Vec2::splat(1.0));
    let aspect = cell.x.max(1.0) / cell.y.max(1.0);
    let shown = if size.x / size.y > aspect {
        Vec2::new(size.y * aspect, size.y)
    } else {
        Vec2::new(size.x, size.x / aspect)
    };
    let margin = (size - shown) / 2.0;
    Rect::from_min_max(
        (margin / size).to_pos2(),
        ((margin + shown) / size).to_pos2(),
    )
}

/// A message's files: a preview for pictures, a card with Download for the rest.
/// Small animated GIFs and WebPs play right in the message, loaded by themselves;
/// bigger ones show their first frame with a badge and play in the viewer.
/// A message's files; `links` are its text's link destinations, which a link preview
/// must be about.
pub fn attachments(
    ui: &mut Ui,
    files: &mut Files,
    holder: (&Url, &UnboundedSender<Command>),
    list: &[Attachment],
    links: &[String],
) {
    let shown = shown_attachments(ui, files, holder, list, links);
    media_menus(files, shown);
}

/// `attachments`, leaving the pictures' menus to the caller, which adds its own items.
pub fn shown_attachments(
    ui: &mut Ui,
    files: &mut Files,
    (address, commands): (&Url, &UnboundedSender<Command>),
    list: &[Attachment],
    links: &[String],
) -> Vec<Shown> {
    let mut shown_pictures = Vec::new();
    files.sensitive.poll();
    let media = |file: &Attachment, saved: Option<&PathBuf>| Media {
        address: address.clone(),
        commands: commands.clone(),
        file: file.clone(),
        saved: saved.cloned(),
        blurred: false,
    };
    let (cards, list): (Vec<&Attachment>, Vec<&Attachment>) =
        list.iter().partition(|file| is_card(file));
    // Emoji and stickers from other servers are drawn inline, not as files.
    let (pictures, list): (Vec<&Attachment>, Vec<&Attachment>) = list
        .into_iter()
        .partition(|file| sticker_files::of(file).is_some());
    for file in pictures {
        if let Some(response) = sticker_file(ui, files, (address, commands), file) {
            shown_pictures.push(Shown {
                response,
                media: media(file, files.saved_at(file)),
            });
        }
    }
    // Several pictures or videos show as one album; other files stay rows below it.
    let in_album = |file: &Attachment| {
        media_kind(file).filter(|kind| matches!(kind, MediaKind::Picture | MediaKind::Video))
    };
    let together: Vec<(&Attachment, MediaKind)> = (list.iter())
        .filter_map(|file| Some((*file, in_album(file)?)))
        .collect();
    let list = if together.len() > 1 {
        shown_pictures.extend(album(ui, files, (address, commands), &together));
        list.into_iter()
            .filter(|file| in_album(file).is_none())
            .collect()
    } else {
        list
    };
    for file in list {
        let hidden = files.sensitive.hidden(file);
        let play = (hidden.is_none()
            && plays_inline(&file.name, &file.media_type, file.file.size.into()))
            || shows_original(file);
        // Files sent before thumbnails existed for GIFs, or large photos older apps
        // could not make one for, have none; they show their original instead.
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
        if let Some(thumb) = file.thumbnail.as_ref().filter(|_| !playing)
            && ui.is_rect_visible(Rect::from_min_size(ui.cursor().min, Vec2::splat(THUMBNAIL)))
        {
            let now = ui.input(|input| input.time);
            want_thumbnail(&mut files.images, now, commands, thumb);
        }
        let picture = if playing {
            inline
        } else {
            (file.thumbnail.as_ref())
                .and_then(|thumb| files.images.get(&thumb.id))
                .map(|thumb| &thumb.picture)
        };
        let (mut show, mut retry) = (false, false);
        match picture {
            Some(Picture::Ready(anim)) if hidden.is_some() => {
                let size = anim.frames[0].size_vec2();
                // As big as the picture would show: within the thumbnail and the chat.
                let fit = (THUMBNAIL / size.max_elem().max(1.0))
                    .min(ui.available_width() / size.x.max(1.0))
                    .min(1.0);
                let size = size * fit;
                let (rect, shown) = ui.allocate_exact_size(size, Sense::click());
                let hidden = hidden.unwrap_or(Hidden::Checking);
                let radius = CornerRadius::same(ui::RADIUS);
                paint_blurred(ui, anim.blurred.as_ref(), rect, radius, hidden);
                let label = media_label(file, Some(hidden));
                shown.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
                let shown = shown.on_hover_text(t!("sensitive.tap_to_show"));
                if shown.clicked() {
                    if hidden == Hidden::Sensitive {
                        files.sensitive.reveal(file);
                    } else {
                        show = true;
                    }
                }
                shown_pictures.push(Shown {
                    response: shown,
                    media: Media {
                        blurred: hidden == Hidden::Sensitive,
                        ..media(file, files.saved_at(file))
                    },
                });
            }
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
                shown_pictures.push(Shown {
                    response: shown,
                    media: media(file, files.saved_at(file)),
                });
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
            files.open_viewer(address, commands, file);
        }
        if retry && let Some(thumb) = &file.thumbnail {
            files.images.remove(&thumb.id);
        }
    }
    // One preview per message, as senders make.
    if let Some(card) = cards.first() {
        link_card(ui, files, commands, card, links);
    }
    shown_pictures
}

/// An emoji or sticker file of another server: loaded when on screen. A sticker is drawn
/// here; an emoji is drawn by the message text that uses it. A file that fails the checks
/// shows as an ordinary file. Returns a drawn sticker, for its menu.
fn sticker_file(
    ui: &mut Ui,
    files: &mut Files,
    (address, commands): (&Url, &UnboundedSender<Command>),
    file: &Attachment,
) -> Option<Response> {
    let (kind, name) = sticker_files::of(file)?;
    let id = file.file.id;
    let area = Rect::from_min_size(ui.cursor().min, Vec2::splat(crate::home::STICKER_SIZE));
    let visible = match kind {
        // Inline emoji render in the message text already laid out above this cursor;
        // on the last message of a scrolled chat, `ui.cursor().min.y` sits past the
        // clip rect by `item_spacing.y`.
        sticker_files::Kind::Emoji if ui.min_rect().height() > 0.0 => ui.min_rect(),
        _ => {
            let mut rect = area;
            rect.min.y -= ui.spacing().item_spacing.y;
            rect
        }
    };
    if sticker_files::state(ui.ctx(), &id).is_none() && ui.is_rect_visible(visible) {
        sticker_files::set(ui.ctx(), &id, sticker_files::State::Loading);
        let _ = commands.send(Command::StickerFile(file.file.clone(), kind));
    }
    match sticker_files::state(ui.ctx(), &id) {
        Some(sticker_files::State::Failed) => card(ui, files, address, file),
        Some(sticker_files::State::Ready(picture)) if kind == sticker_files::Kind::Sticker => {
            let code = format!(":{name}:");
            let sticker = crate::home::emoji_image(
                ui,
                &picture,
                crate::home::STICKER_SIZE,
                &t!("home.sticker", code = code),
            )
            .on_hover_text(code);
            return Some(sticker.interact(Sense::click()));
        }
        Some(sticker_files::State::Loading) if kind == sticker_files::Kind::Sticker => {
            ui.allocate_ui(Vec2::splat(64.0), |ui| ui.spinner());
        }
        _ => {}
    }
    None
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
    if let Some(thumb) = &file.thumbnail
        && ui.is_rect_visible(area)
    {
        let now = ui.input(|input| input.time);
        want_thumbnail(&mut files.images, now, commands, thumb);
    }
    let picture = (file.thumbnail.as_ref()).and_then(|thumb| match files.images.get(&thumb.id) {
        Some(Inline {
            picture: Picture::Ready(anim),
            ..
        }) => anim.frames.first(),
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

/// Start loading a thumbnail whose spot is on screen at `now` (egui time) and mark it
/// seen; when the cache is full, the thumbnails unseen longest make room first.
fn want_thumbnail(
    images: &mut HashMap<Id, Inline>,
    now: f64,
    commands: &UnboundedSender<Command>,
    thumb: &BlobRef,
) {
    if !images.contains_key(&thumb.id) {
        bound_pictures(images, now);
    }
    let entry = images.entry(thumb.id).or_insert_with(|| {
        let _ = commands.send(Command::Image(thumb.clone(), Fetch::Thumbnail));
        Inline {
            picture: Picture::Loading,
            seen: now,
        }
    });
    entry.seen = now;
}

/// Make room for one more thumbnail once `MAX_PICTURES` are kept: drop the ones unseen
/// longest, `PICTURES_SLACK` at a time so this runs rarely. Ones seen at `now` (this
/// frame) always stay, so a thumbnail on screen is never dropped and asked for again.
fn bound_pictures(images: &mut HashMap<Id, Inline>, now: f64) {
    if images.len() < MAX_PICTURES {
        return;
    }
    let mut old: Vec<(f64, Id)> = (images.iter())
        .filter(|(_, thumb)| thumb.seen < now)
        .map(|(id, thumb)| (thumb.seen, *id))
        .collect();
    old.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let extra = images.len() + 1 + PICTURES_SLACK - MAX_PICTURES;
    for (_, id) in old.into_iter().take(extra) {
        images.remove(&id);
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
    if SAVE_DIALOG && ui::secondary(ui, t!("files.download")).clicked() {
        files.ask(ui.ctx(), Ask::Save(address.clone(), file.clone()));
    }
    if let Some(label) = export_label()
        && ui::secondary(ui, label).clicked()
    {
        files.export(ui.ctx(), address, file);
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
    // From a gallery: where this picture is in it, its caption and message.
    let album = (view.album.as_ref()).map(|(album, at)| (*at, album.len(), album[*at].clone()));
    let (mut close, mut open_it, mut jump) = (false, false, false);
    let mut step = 0isize;
    if album.is_some() {
        ctx.input_mut(|input| {
            if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
                step = -1;
            }
            if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
                step = 1;
            }
        });
    }
    let modal = Modal::new(UiId::unique("viewer"))
        .frame(Frame::NONE)
        .backdrop_color(OVERLAY)
        .show(ctx, |ui| {
            let screen = ctx.content_rect();
            ui.set_min_size(screen.size());
            ui.set_max_size(screen.size());
            let backdrop = ui.interact(screen, ui.scope_id().with("dismiss"), Sense::click());
            Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| {
                // On a phone the tools take the whole row: the name goes below them.
                let narrow = ui::is_phone(ctx);
                ui.horizontal(|ui| {
                    if !narrow {
                        ui.label(ui::bold(&file.name).color(ON_FILL));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = tool(ui, "×", t!("common.close")).clicked();
                        ui.add_space(12.0);
                        if album
                            .as_ref()
                            .is_some_and(|(_, _, shot)| shot.jump.is_some())
                        {
                            jump = tool(ui, "💬", t!("gallery.jump")).clicked();
                        }
                        open_it = OPENS && tool(ui, "↗", t!("files.open_default")).clicked();
                        if SAVE_DIALOG && tool(ui, "⬇", t!("common.save")).clicked() {
                            files.ask(ui.ctx(), Ask::Save(address.clone(), file.clone()));
                        }
                        if let Some(label) = export_label()
                            && tool(ui, "⬇", label).clicked()
                        {
                            files.export(ui.ctx(), &address, &file);
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
                if narrow {
                    ui.add_space(8.0);
                    ui.add(egui::Label::new(ui::bold(&file.name).color(ON_FILL)).truncate());
                }
                if let Some((_, _, shot)) = album.as_ref().filter(|(.., s)| !s.caption.is_empty()) {
                    ui.label(ui::caption(&shot.caption).color(OVERLAY_TEXT));
                }
                ui.add_space(8.0);
                let room = ui.available_size();
                let stage = ui.available_rect_before_wrap();
                let view = files.viewer.as_ref().expect("checked above");
                files.sensitive.poll();
                let hidden = files.sensitive.hidden_in(&file, view.nsfw);
                let mut picture = None;
                match files.full.as_ref().map(|full| &full.picture) {
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
                            picture = Some(ui.interact(
                                displayed,
                                ui.scope_id().with("image"),
                                Sense::click(),
                            ));
                            let unturned = Rect::from_center_size(rect.center(), size * scale);
                            if let Some(hidden) = hidden {
                                let blurred = files.blurred_of(&file);
                                paint_blurred(ui, blurred, displayed, CornerRadius::ZERO, hidden);
                            } else {
                                Image::new(texture)
                                    .rotate(f32::from(view.turns) * FRAC_PI_2, Vec2::splat(0.5))
                                    .paint_at(ui, unturned);
                            }
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
                // The picture's menu: right click, or a long press on a touch screen.
                if let Some(picture) = picture {
                    let blurred = hidden == Some(Hidden::Sensitive);
                    if blurred {
                        let label = media_label(&file, hidden);
                        picture.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
                    }
                    if picture.clicked() && blurred {
                        files.sensitive.reveal(&file);
                    }
                    let media = Media {
                        address: address.clone(),
                        commands: commands.clone(),
                        saved: files.saved_at(&file).cloned(),
                        file: file.clone(),
                        blurred,
                    };
                    let picked = ui::context_menu(&picture, |m| {
                        let action = copy_save_items(m);
                        // Phones can't copy pictures or open a save dialog: they hand
                        // the file to the system, as the toolbar's button does.
                        let export =
                            export_label().is_some_and(|label| m.item("⬇", label).clicked());
                        (blur_item(m, &media).or(action), export)
                    });
                    match picked {
                        Some((Some(action), _)) => {
                            files.media_action(ctx, action, &media);
                        }
                        Some((None, true)) => files.export(ctx, &address, &file),
                        _ => {}
                    }
                }
                // Previous and next through the gallery, over the picture's sides: always
                // on a touch screen, while the pointer is over the window on a desktop.
                // A swipe sideways steps too, unless zoomed in, where dragging pans.
                let (touch, pointer, origin, released, latest) = ctx.input(|input| {
                    (
                        ui::TOUCH || input.has_touch_screen(),
                        input.pointer.hover_pos().is_some(),
                        input.pointer.press_origin(),
                        input.pointer.primary_released(),
                        input.pointer.latest_pos(),
                    )
                });
                // egui forgets where a press began on its release, so keep it here.
                let key = UiId::unique("viewer swipe");
                let from = ctx.data_mut(|data| match origin {
                    Some(origin) => {
                        data.insert_temp(key, origin);
                        None
                    }
                    None => data.remove_temp::<egui::Pos2>(key),
                });
                let swipe = (from.filter(|_| released).zip(latest)).map(|(from, to)| to - from);
                let zoomed = files.viewer.as_ref().is_some_and(|view| view.zoom > 1.0);
                if let Some(moved) = swipe.filter(|_| album.is_some() && !zoomed)
                    && moved.x.abs() > SWIPE
                    && moved.x.abs() > moved.y.abs() * 2.0
                {
                    step = if moved.x < 0.0 { 1 } else { -1 };
                }
                if let Some((at, count, _)) = album.as_ref().filter(|(_, count, _)| *count > 1) {
                    let text = t!("gallery.position", at = at + 1, total = count);
                    let font = egui::FontId::proportional(ui::TEXT_SMALL);
                    let size = (ui.painter())
                        .layout_no_wrap(text.clone(), font.clone(), ON_FILL)
                        .size();
                    let place = Rect::from_center_size(
                        egui::pos2(stage.center().x, stage.bottom() - 20.0),
                        size + Vec2::new(16.0, 6.0),
                    );
                    ui.painter()
                        .rect_filled(place, ui::RADIUS_WINDOW, OVERLAY_BUTTON);
                    let text = RichText::new(text).font(font).color(ON_FILL);
                    ui.put(place, egui::Label::new(text).selectable(false));
                }
                if let Some((at, count, _)) = album.as_ref().filter(|_| touch || pointer) {
                    let side = Vec2::splat(36.0);
                    let middle = stage.center().y - side.y / 2.0;
                    let arrows = [
                        (*at > 0, stage.left(), "⏴", t!("gallery.previous"), -1),
                        (
                            at + 1 < *count,
                            stage.right() - side.x,
                            "⏵",
                            t!("gallery.next"),
                            1,
                        ),
                    ];
                    for (shown, x, icon, label, by) in arrows {
                        let rect = Rect::from_min_size(egui::pos2(x, middle), side);
                        if shown
                            && ui
                                .scope_builder(UiBuilder::new().max_rect(rect), |ui| {
                                    tool(ui, icon, label)
                                })
                                .inner
                                .clicked()
                        {
                            step = by;
                        }
                    }
                }
            });
            close |= backdrop.clicked();
        });
    if open_it {
        files.open_file(&commands, &file);
    }
    if jump && let Some((_, _, shot)) = &album {
        files.jump = shot.jump;
        close = true;
    }
    if close || modal.should_close() {
        files.viewer = None;
    } else if step != 0
        && let Some((album, at)) = files.viewer.as_mut().and_then(|view| view.album.take())
    {
        let next = at.saturating_add_signed(step).min(album.len() - 1);
        files.open_album(album, next);
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
                    let address = Url::parse("https://localhost/").unwrap();
                    let file = Attachment {
                        file: BlobRef::for_tests([1; 32]),
                        name: "Test image".into(),
                        media_type: "image/png".into(),
                        thumbnail: None,
                    };
                    let (commands, _sent) = tokio::sync::mpsc::unbounded_channel();
                    files.open_viewer(&address, &commands, &file);
                    files.full.as_mut().unwrap().picture = Picture::Ready(Anim {
                        frames: vec![texture],
                        delays: vec![100],
                        blurred: None,
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
                assert!(matches!(
                    harness.state().full.as_ref().unwrap().picture,
                    Picture::Ready(_)
                ));
            }
        }
    }

    fn painted_image(harness: &Harness<'_, Files>) -> Rect {
        let Picture::Ready(anim) = &harness.state().full.as_ref().unwrap().picture else {
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
    fn viewer_reuses_only_the_last_full_picture_and_scopes_pending_requests() {
        let ctx = egui::Context::default();
        let data = tempfile::tempdir().unwrap();
        let mut files = Files::new(data.path());
        let first = Url::parse("https://first.example.org/").unwrap();
        let second = Url::parse("https://second.example.org/").unwrap();
        let (commands, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let file = Attachment {
            file: BlobRef::for_tests([1; 32]),
            name: "Picture".into(),
            media_type: "image/png".into(),
            thumbnail: None,
        };

        files.open_viewer(&first, &commands, &file);
        files.open_viewer(&second, &commands, &file);
        files.open_viewer(&first, &commands, &file);
        assert_eq!(std::iter::from_fn(|| sent.try_recv().ok()).count(), 2);

        let frame = || {
            Ok(vec![(
                egui::ColorImage::filled([2, 2], egui::Color32::RED),
                0,
            )])
        };
        files.image(&ctx, &second, &file.file, Fetch::Full, frame());
        assert_eq!(
            ctx.tex_manager().read().num_allocated(),
            1,
            "only the font texture"
        );
        assert!(matches!(
            files.full.as_ref().unwrap().picture,
            Picture::Loading
        ));
        files.image(&ctx, &first, &file.file, Fetch::Full, frame());
        assert!(matches!(
            files.full.as_ref().unwrap().picture,
            Picture::Ready(_)
        ));
        files.open_viewer(&second, &commands, &file);
        assert!(matches!(
            sent.try_recv(),
            Ok(Command::Image(_, Fetch::Full))
        ));
        assert_eq!(
            ctx.tex_manager().read().num_allocated(),
            1,
            "previous image released"
        );
        files.forget_full_pictures(&second);
        files.image(&ctx, &second, &file.file, Fetch::Full, frame());
        assert!(files.full.is_none());
        assert!(files.viewer.is_none());
        assert!(files.full_pending.is_empty());
    }

    fn thumb_id(n: usize) -> Id {
        let mut id = [0; 32];
        id[..8].copy_from_slice(&(n as u64).to_le_bytes());
        id
    }

    #[test]
    fn thumbnails_unseen_longest_go_first_and_visible_ones_stay() {
        // 250 thumbnails: 220 last seen at times 0..220, 30 on screen now.
        let now = 1000.0;
        let mut images: HashMap<Id, Inline> = (0..250)
            .map(|n| {
                let seen = if n >= 220 { now } else { n as f64 };
                let picture = if n % 2 == 0 {
                    Picture::Failed
                } else {
                    Picture::Loading
                };
                (thumb_id(n), Inline { picture, seen })
            })
            .collect();
        bound_pictures(&mut images, now);
        assert_eq!(images.len(), MAX_PICTURES - PICTURES_SLACK - 1);
        assert!(
            (220..250).all(|n| images.contains_key(&thumb_id(n))),
            "visible stay"
        );
        let dropped = 250 - images.len();
        assert!(
            (0..dropped).all(|n| !images.contains_key(&thumb_id(n))),
            "oldest go"
        );
        assert!(
            (dropped..250).all(|n| images.contains_key(&thumb_id(n))),
            "newer stay"
        );
        // Under the bound nothing goes.
        let before = images.len();
        bound_pictures(&mut images, now + 1.0);
        assert_eq!(images.len(), before);
    }

    #[test]
    fn a_full_cache_never_reloads_visible_thumbnails() {
        let ctx = egui::Context::default();
        let data = tempfile::tempdir().unwrap();
        let mut files = Files::new(data.path());
        let address = Url::parse("https://localhost/").unwrap();
        let (commands, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let blob = |n| BlobRef::for_tests(thumb_id(n));
        let frame = || {
            Ok(vec![(
                egui::ColorImage::filled([2, 2], egui::Color32::RED),
                0,
            )])
        };
        // Scroll through 600 thumbnails, 30 on screen per frame, each loading as it
        // is first shown; then hold still for a few frames.
        let mut requested = 0;
        for step in 0..600 {
            let now = step as f64;
            let shown = step.min(570)..step.min(570) + 30;
            for n in shown.clone() {
                want_thumbnail(&mut files.images, now, &commands, &blob(n));
            }
            while let Ok(Command::Image(thumb, Fetch::Thumbnail)) = sent.try_recv() {
                requested += 1;
                files.image(&ctx, &address, &thumb, Fetch::Thumbnail, frame());
            }
            assert!(files.images.len() <= MAX_PICTURES, "bounded");
            for n in shown {
                assert!(
                    matches!(files.images[&thumb_id(n)].picture, Picture::Ready(_)),
                    "thumbnail {n} on screen stays loaded"
                );
            }
        }
        assert_eq!(requested, 600, "each thumbnail downloaded once");
    }

    #[test]
    fn link_previews_stay_bounded() {
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
    fn photos_sent_without_a_thumbnail_show_their_original() {
        let file = |name: &str, size, thumbnail: bool| Attachment {
            file: {
                let mut blob = rusttalk_core::client::BlobRef::for_tests([1; 32]);
                blob.size = size;
                blob
            },
            name: name.into(),
            media_type: "image/jpeg".into(),
            thumbnail: thumbnail.then(|| rusttalk_core::client::BlobRef::for_tests([2; 32])),
        };
        let size = 6 * MIB as u32;
        assert!(shows_original(&file("IMG_2676.jpeg", size, false)));
        assert!(!shows_original(&file("IMG_2676.jpeg", size, true)));
        assert!(!shows_original(&file("IMG_2676.HEIC", size, false)));
        assert!(!shows_original(&file(
            "big.jpg",
            ORIGINAL_MAX as u32 + 1,
            false
        )));
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

    #[test]
    fn picked_files_keep_a_safe_version_of_their_name() {
        let data = tempfile::tempdir().unwrap();
        let from = data.path().join("picked");
        fs::write(&from, b"synthetic").unwrap();
        for (name, kept) in [("a/b\\c.png", "a_b_c.png"), (" ", "file"), ("..", "file")] {
            let path = keep_picked(&from, name).unwrap();
            assert_eq!(path.file_name().unwrap(), kept);
            let folder = path.parent().unwrap();
            assert!(is_temp_folder(folder.file_name().unwrap()));
            discard(&path);
            assert!(!folder.exists());
        }
        assert!(keep_picked(&data.path().join("gone"), "gone.png").is_err());
    }
}
