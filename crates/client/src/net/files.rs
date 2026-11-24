//! Stored files: upload, download, save, and plain words for what went wrong.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use eframe::egui;
use reqwest::StatusCode;
use rusttalk_core::{
    client::{
        Attachment, BlobRef, FileClient, KeyClient, MessageBody, MessageError, Progress,
        UploadRejected, decode_frames, decode_thumbnail,
    },
    messages::fit_file_name,
};
use tokio::{
    sync::Semaphore,
    task::{self, JoinSet},
};
use url::Url;
use zeroize::Zeroizing;

use sha2::Digest as _;

use super::{Command, Ctx, Draft, Fetch, Queued, State, Update, directory::frame_image};
use crate::{
    link_cards::{Card, FILE_NAME, MAX_CARD_BYTES, MEDIA_TYPE, Made},
    server::Id,
    sticker_files::{Inline, Kind},
};
use rusttalk_core::client::prepare_file;
use std::time::Instant;

/// At most this many downloads run at once per server; the others wait their turn.
const DOWNLOADS: usize = 4;

/// File transfers running in tasks of their own, so this server's chat, presence and
/// voice go on meanwhile. Only the loop changes these, and what the tasks feed into
/// (`ImageOriginals`, sent messages); a task just hands back its result.
pub(super) struct Jobs {
    tasks: JoinSet<Done>,
    downloads: Arc<Semaphore>,
    /// Downloads running or waiting their turn, to ask again after a reconnect.
    fetching: HashMap<task::Id, Download>,
    /// Messages with files uploading, by `lane`, with the ones sent after them.
    lanes: HashMap<Id, Lane>,
}

/// A download to ask for again. Not a `Command`: some commands (a screen to share)
/// can't be shared between threads on Windows, and `State` is borrowed across awaits.
pub(super) enum Download {
    Image(BlobRef, Fetch),
    LinkCard(BlobRef),
    StickerFile(BlobRef, Kind),
    Save(BlobRef, PathBuf),
}

impl From<Download> for Command {
    fn from(download: Download) -> Self {
        match download {
            Download::Image(file, fetch) => Command::Image(file, fetch),
            Download::LinkCard(file) => Command::LinkCard(file),
            Download::StickerFile(file, kind) => Command::StickerFile(file, kind),
            Download::Save(file, path) => Command::Save(file, path),
        }
    }
}

enum Done {
    /// A picture's original to keep, when it should be.
    Fetched(Option<Original>),
    Uploaded(Box<Upload>),
}

/// The message body with its uploaded files, and what to remove after sending; `None`
/// when an upload failed (removed again, and the failure shown).
pub(super) type Upload = (MessageBody, Option<Vec<Attachment>>);

struct Lane {
    task: task::Id,
    draft: Draft,
    target: Target,
    /// Counts the message as sending until it is done with.
    sent: Queued,
    /// Sent after this one: they wait, so they can't arrive before it.
    waiting: VecDeque<Draft>,
}

/// A message whose files finished uploading, or failed to.
pub(super) struct Finished {
    pub draft: Draft,
    pub target: Target,
    /// `None` when the upload task itself stopped (a bug): nothing was sent.
    pub upload: Option<Upload>,
    /// Messages to send now, in the order they were sent.
    pub waiting: VecDeque<Draft>,
    pub _sent: Queued,
}

impl Default for Jobs {
    fn default() -> Self {
        Self {
            tasks: JoinSet::new(),
            downloads: Arc::new(Semaphore::new(DOWNLOADS)),
            fetching: HashMap::new(),
            lanes: HashMap::new(),
        }
    }
}

impl Jobs {
    /// Run a download once there is room for it; `again` asks for it once more.
    pub(super) fn download(
        &mut self,
        again: Download,
        work: impl Future<Output = Option<Original>> + Send + 'static,
    ) {
        let downloads = self.downloads.clone();
        let task = self.tasks.spawn(async move {
            let _turn = downloads.acquire_owned().await;
            Done::Fetched(work.await)
        });
        self.fetching.insert(task.id(), again);
    }

    /// The draft back when it can go now; kept to go after the message uploading
    /// ahead of it in its lane otherwise.
    pub(super) fn wait(&mut self, draft: Draft) -> Option<Draft> {
        match self.lanes.get_mut(&lane(&draft)) {
            Some(busy) => {
                busy.waiting.push_back(draft);
                None
            }
            None => Some(draft),
        }
    }

    /// Upload a message's files; `next` hands it back to send. Uploads take no turn
    /// from downloads: one per lane at most, and a message never waits behind them.
    pub(super) fn upload(
        &mut self,
        draft: Draft,
        sent: Queued,
        target: Target,
        work: impl Future<Output = Upload> + Send + 'static,
    ) {
        let task = self
            .tasks
            .spawn(async move { Done::Uploaded(Box::new(work.await)) });
        let lane = Lane {
            task: task.id(),
            draft,
            target,
            sent,
            waiting: VecDeque::new(),
        };
        self.lanes.insert(self::lane(&lane.draft), lane);
    }

    pub(super) fn busy(&self) -> bool {
        !self.tasks.is_empty()
    }

    /// The next finished upload; `None` for a finished download, whose picture
    /// original (if any) goes into `originals` here, on the loop.
    pub(super) async fn next(&mut self, originals: &mut ImageOriginals) -> Option<Finished> {
        let (task, done) = match self.tasks.join_next_with_id().await? {
            Ok((task, done)) => (task, Some(done)),
            Err(error) => {
                tracing::error!(%error, "a file transfer stopped");
                (error.id(), None)
            }
        };
        if self.fetching.remove(&task).is_some() {
            if let Some(Done::Fetched(Some(original))) = done {
                originals.remember(original);
            }
            return None;
        }
        let lane = (self.lanes.iter())
            .find(|(_, lane)| lane.task == task)
            .map(|(lane, _)| *lane)?;
        let Lane {
            draft,
            target,
            sent,
            waiting,
            ..
        } = self.lanes.remove(&lane)?;
        let upload = match done {
            Some(Done::Uploaded(upload)) => Some(*upload),
            _ => None,
        };
        Some(Finished {
            draft,
            target,
            upload,
            waiting,
            _sent: sent,
        })
    }

    /// Stop every transfer, with the commands to ask for them again: each message
    /// still uploading first, then the ones that waited for it.
    pub(super) fn stop(&mut self) -> Vec<Command> {
        // Dropping the old set aborts its tasks.
        let Self {
            fetching, lanes, ..
        } = std::mem::take(self);
        let mut again: Vec<Command> = fetching.into_values().map(Command::from).collect();
        for Lane {
            mut draft,
            sent,
            waiting,
            ..
        } in lanes.into_values()
        {
            draft.queued = sent;
            again.push(Command::Send(draft));
            again.extend(waiting.into_iter().map(Command::Send));
        }
        again
    }
}

/// Messages in the same channel, or to the same person, go out in the order sent.
fn lane(draft: &Draft) -> Id {
    draft.peer.unwrap_or(draft.channel)
}

/// Channel, message ID, whether it is a page, whether it is a forum post.
pub(super) type Target = (Id, Id, bool, bool);

/// Files to upload with a message: picked files, a link preview, other servers' emoji.
pub(super) type Uploads = (Vec<PathBuf>, Option<Arc<Made>>, Vec<Inline>);

/// Upload the files into `body`; with what to remove after sending (a preview that
/// didn't make it), or `None` when an upload failed.
/// A link preview that fails to upload is left out, without failing the message.
/// Files already in `body` are kept ones (see `Draft::keep`): never removed here.
pub(super) async fn upload_all(
    ctx: &Ctx,
    url: &Url,
    client: &FileClient,
    channel: Id,
    body: &mut MessageBody,
    (files, preview, inline): Uploads,
) -> Option<Vec<Attachment>> {
    let kept = body.attachments.len();
    if let Some(first) = files.first() {
        let label = match files.len() {
            1 => file_name(first),
            count => t!("files.count", count = count),
        };
        let count = files.len() as f32;
        let shown = {
            let (ctx, url) = (ctx.clone(), url.clone());
            move |done: f32| {
                let what = Some((label.clone(), done / count));
                ctx.send(Update::Uploading(url.clone(), channel, what));
            }
        };
        shown(0.0);
        let uploaded = upload(client, &channel, files, &mut body.attachments, shown).await;
        ctx.send(Update::Uploading(url.clone(), channel, None));
        if let Err(error) = uploaded {
            tracing::warn!(error = %format_args!("{error:#}"), "uploading a file failed");
            discard(client, &body.attachments[kept..]).await;
            ctx.send(Update::Failed(upload_problem(&error)));
            return None;
        }
    }
    // A preview that didn't make it is removed only after the message went out.
    let mut leftover = Vec::new();
    if let Some(made) = preview.filter(|_| body.attachments.len() < crate::files::MAX_FILES) {
        match upload_preview(client, &channel, made).await {
            Preview::Sent(attachment) => body.attachments.push(attachment),
            Preview::Left(prepared) => leftover.extend(prepared),
        }
    }
    // Emoji and stickers from other servers go along as small files of their own type.
    for picture in inline
        .iter()
        .take(crate::files::MAX_FILES.saturating_sub(body.attachments.len()))
    {
        match upload_inline(client, &channel, picture).await {
            Ok(attachment) => body.attachments.push(attachment),
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "uploading an emoji file failed");
                discard(client, &body.attachments[kept..]).await;
                at_most(CLEANUP, discard(client, &leftover)).await;
                ctx.send(Update::Failed(upload_problem(&error)));
                return None;
            }
        }
    }
    Some(leftover)
}

/// Send the message with its uploaded files (those after the `kept` ones); `false`
/// when it didn't go. A DM the other person re-keyed just now is sent once more.
#[allow(clippy::too_many_arguments)]
pub(super) async fn post(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    (channel, id, page, forum_post): Target,
    dm: bool,
    mut body: MessageBody,
    (kept, mut leftover): (usize, Vec<Attachment>),
) -> Result<bool> {
    let mut posted = post_once(client, &channel, id, page, &body).await;
    // The other person re-keyed the DM just now: take the new key and send once more,
    // whatever this was (text, sticker, file or friend message).
    if dm && posted.as_ref().is_err_and(conflict) {
        posted = match client.sync().await {
            Ok(()) => post_once(client, &channel, id, page, &body).await,
            Err(error) => Err(error),
        };
    }
    let (record, posted_body) = match posted {
        Ok(record) => {
            let opened = client.open_message(&channel, &record).await?;
            (record, opened)
        }
        Err(error) if forum_post => {
            // A lost response can hide a successful commit. Verify the original
            // ID on this device before treating a retry as delivered.
            let record = match client.message(&channel, &id).await {
                Ok(record)
                    if record.person == *ctx.identity.certificate.person_id()
                        && record.sender_device
                            == ctx.identity.certificate.signing_key().to_bytes() =>
                {
                    record
                }
                result => {
                    // When offline we cannot tell whether the record references
                    // the preview. Let unreferenced uploads expire in that case.
                    if result.is_err_and(|e| gone(&e)) {
                        leftover.extend(body.attachments.drain(kept..));
                    }
                    at_most(CLEANUP, discard(&client.file_client(), &leftover)).await;
                    return Err(error);
                }
            };
            let opened = client.open_message(&channel, &record).await?;
            leftover.extend(
                body.attachments
                    .drain(kept..)
                    .filter(|a| !opened.attachments.contains(a)),
            );
            (record, opened)
        }
        Err(error) => {
            let files = client.file_client();
            discard(&files, &body.attachments[kept..]).await;
            at_most(CLEANUP, discard(&files, &leftover)).await;
            return Err(error);
        }
    };
    state.sent(ctx, url, channel, &record, &posted_body);
    at_most(CLEANUP, discard(&client.file_client(), &leftover)).await;
    Ok(true)
}

async fn post_once(
    client: &mut KeyClient<'_>,
    channel: &Id,
    id: Id,
    page: bool,
    body: &MessageBody,
) -> Result<rusttalk_core::client::MessageRecord> {
    match page {
        true => client.post_page(channel, id, body).await,
        false => client.post_message(channel, id, body).await,
    }
}

/// Upload another server's emoji or sticker as a small file (no thumbnail) on this one.
async fn upload_inline(client: &FileClient, channel: &Id, picture: &Inline) -> Result<Attachment> {
    let (attachment, _) = prepare_file(
        &picture.file_name(),
        picture.kind.media_type(),
        &picture.bytes,
        &[],
    )?;
    client
        .upload_prepared(channel, &attachment, &picture.bytes, None, None)
        .await?;
    Ok(attachment)
}

/// How long removing a preview that didn't make it may hold up this server's loop.
const CLEANUP: std::time::Duration = std::time::Duration::from_secs(5);

/// Run `work`, giving up after `limit`.
async fn at_most(limit: std::time::Duration, work: impl Future<Output = ()>) {
    if tokio::time::timeout(limit, work).await.is_err() {
        tracing::info!("removing an unsent link preview took too long");
    }
}

/// Upload a link preview within what is left of its time. Otherwise the message goes
/// without it; the error holds the prepared file (when there was one) to remove,
/// since part of it, or all of it with a lost answer, may be stored.
async fn upload_preview(client: &FileClient, channel: &Id, made: Arc<Made>) -> Preview {
    let Some(json) = preview_file(&made) else {
        return Preview::Left(None);
    };
    let prepare = {
        let (json, made) = (json.clone(), made.clone());
        // The thumbnail decodes the page's picture: with limits, off the async workers.
        async move {
            tokio::task::spawn_blocking(move || {
                prepare_file(
                    FILE_NAME,
                    MEDIA_TYPE,
                    &json,
                    made.picture.as_deref().unwrap_or_default(),
                )
            })
            .await
            .ok()?
            .ok()
        }
    };
    let upload = |attachment: Attachment, thumbnail: Option<Vec<u8>>| {
        let json = &json;
        async move {
            client
                .upload_prepared(channel, &attachment, json, thumbnail.as_deref(), None)
                .await
        }
    };
    within(made.deadline, prepare, upload).await
}

/// How a link preview's upload went.
enum Preview {
    Sent(Attachment),
    /// Left out; the prepared file, if it got that far, is removed after sending.
    Left(Option<Attachment>),
}

/// Prepare and upload under one deadline: the uploaded file, or the prepared one (if
/// it got that far) to remove after a failure or when time ran out.
async fn within<F>(
    deadline: Instant,
    prepare: impl Future<Output = Option<(Attachment, Option<Vec<u8>>)>>,
    upload: impl FnOnce(Attachment, Option<Vec<u8>>) -> F,
) -> Preview
where
    F: Future<Output = Result<()>>,
{
    let mut prepared = None;
    let work = async {
        let (attachment, thumbnail) = prepare.await?;
        prepared = Some(attachment.clone());
        match upload(attachment.clone(), thumbnail).await {
            Ok(()) => Some(attachment),
            Err(error) => {
                tracing::info!(error = %format_args!("{error:#}"), "uploading a link preview failed");
                None
            }
        }
    };
    let left = deadline.saturating_duration_since(Instant::now());
    match tokio::time::timeout(left, work).await {
        Ok(Some(attachment)) => Preview::Sent(attachment),
        Ok(None) => Preview::Left(prepared),
        Err(_) => {
            tracing::info!("uploading a link preview took too long");
            Preview::Left(prepared)
        }
    }
}

/// The preview's file, unless it is too big or its time is up.
fn preview_file(made: &Made) -> Option<Vec<u8>> {
    let json = made.card.encode();
    (json.len() <= MAX_CARD_BYTES as usize && Instant::now() < made.deadline).then_some(json)
}

/// Delete a message, then its stored files.
pub(super) async fn delete(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
    state: &mut State,
    (channel, id, sender): (Id, Id, Id),
    files: Vec<Id>,
) -> Result<()> {
    match client.delete_message(&channel, &id).await {
        // Already gone there counts as deleted, so a DM delete can finish.
        Err(error) if !gone(&error) => return Err(error),
        _ => state.deleted(ctx, url, channel, id),
    }
    for file in files {
        if let Err(error) = client.delete_message_file(&file, &sender).await
            && !gone(&error)
        {
            tracing::warn!(error = %format_args!("{error:#}"), "deleting a stored file failed");
        }
    }
    Ok(())
}

/// Download and read a link preview file, refusing anything too big to be one.
pub(super) async fn link_card(ctx: &Ctx, client: &FileClient, file: BlobRef) {
    let id = file.id;
    let card = async {
        anyhow::ensure!(file.size <= MAX_CARD_BYTES, "link preview too big");
        let bytes = client.download_file(&file, None).await?;
        Card::decode(&bytes).context("unreadable link preview")
    };
    let card = card.await.map_err(|error| {
        tracing::info!(error = %format_args!("{error:#}"), "loading a link preview failed");
        gone(&error)
    });
    ctx.send(Update::LinkCard(id, card));
}

/// Download another server's emoji or sticker file, refuse anything an app could not have
/// made, and decode it.
pub(super) async fn sticker_file(ctx: &Ctx, client: &FileClient, file: BlobRef, kind: Kind) {
    let id = file.id;
    let picture = async {
        anyhow::ensure!(
            file.size as usize <= rusttalk_core::keys::IMAGE_PLAINTEXT,
            "emoji file too big"
        );
        let bytes = client.download_file(&file, None).await?;
        tokio::task::spawn_blocking(move || {
            let frames = crate::sticker_files::decode(&bytes, kind)?;
            Ok(crate::server::Picture::new(
                sha2::Sha256::digest(&bytes).into(),
                frames.into_iter().map(frame_image).collect(),
                Arc::new(bytes),
            ))
        })
        .await?
    };
    let picture = picture.await.map_err(|error: anyhow::Error| {
        tracing::info!(error = %format_args!("{error:#}"), "loading an emoji file failed");
        gone(&error)
    });
    ctx.send(Update::StickerFile(id, picture));
}

/// Decoded memory one picture may use: in the viewer, and playing small in the chat.
const FULL_BUDGET: usize = 64 * 1024 * 1024;
const INLINE_BUDGET: usize = 24 * 1024 * 1024;

/// Keep recent inline originals to decode at full size without downloading twice.
const ORIGINALS_BUDGET: usize = 8 * crate::files::MIB as usize;
const ORIGINALS_COUNT: usize = 8;
type ImageBytes = Arc<Zeroizing<Vec<u8>>>;

/// A downloaded original to keep: the `ImageOriginals::generation` it was asked in.
pub(super) type Original = (u64, BlobRef, ImageBytes);

/// What a picture download needs from `ImageOriginals`, taken on the loop.
pub(super) struct Cached {
    generation: u64,
    bytes: Option<ImageBytes>,
}

#[derive(Default)]
pub(super) struct ImageOriginals {
    list: VecDeque<(BlobRef, ImageBytes)>,
    /// Counts `clear`s, so a download asked for before one can't put its original back.
    generation: u64,
}

impl ImageOriginals {
    pub(super) fn clear(&mut self) {
        self.list.clear();
        self.generation += 1;
    }

    /// The original already here for this fetch, if any.
    pub(super) fn lookup(&self, file: &BlobRef, fetch: Fetch) -> Cached {
        let bytes = (fetch != Fetch::Thumbnail)
            .then(|| self.list.iter().find(|(blob, _)| blob == file))
            .flatten()
            .map(|(_, bytes)| Arc::clone(bytes));
        Cached {
            generation: self.generation,
            bytes,
        }
    }

    fn remember(&mut self, (generation, file, bytes): Original) {
        if generation != self.generation {
            return;
        }
        self.list.push_back((file, bytes));
        while self.list.len() > ORIGINALS_COUNT
            || self
                .list
                .iter()
                .map(|(_, bytes)| bytes.len())
                .sum::<usize>()
                > ORIGINALS_BUDGET
        {
            self.list.pop_front();
        }
    }
}

/// Download and decode a picture as asked; with its original to keep, if it should be.
pub(super) async fn image(
    ctx: &Ctx,
    url: &Url,
    client: &FileClient,
    cached: Cached,
    file: BlobRef,
    fetch: Fetch,
) -> Option<Original> {
    let (frames, keep) =
        match image_frames(cached.bytes, fetch, client.download_file(&file, None)).await {
            Ok((frames, keep)) => (Ok(frames), keep),
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "loading a picture failed");
                (Err(gone(&error)), None)
            }
        };
    let original = keep.map(|bytes| (cached.generation, file.clone(), bytes));
    ctx.send(Update::Image(url.clone(), file, fetch, frames));
    original
}

/// Decode `cached`, or else what `download` brings; with the downloaded bytes when
/// they should be kept for a later full-size view.
async fn image_frames(
    cached: Option<ImageBytes>,
    fetch: Fetch,
    download: impl Future<Output = Result<Vec<u8>>>,
) -> Result<(Vec<(egui::ColorImage, u32)>, Option<ImageBytes>)> {
    let remember = fetch == Fetch::Inline && cached.is_none();
    let bytes = match cached {
        Some(bytes) => bytes,
        None => Arc::new(Zeroizing::new(download.await?)),
    };
    let keep = (remember && bytes.len() <= ORIGINALS_BUDGET).then(|| Arc::clone(&bytes));
    // Received pictures are hostile: decode with limits, off the async workers.
    let frames = tokio::task::spawn_blocking(move || -> Result<_> {
        Ok(match fetch {
            Fetch::Thumbnail => vec![still(decode_thumbnail(&bytes)?)],
            Fetch::Full => decode_frames(&bytes, None, FULL_BUDGET)?
                .into_iter()
                .map(frame_image)
                .collect(),
            Fetch::Inline => {
                decode_frames(&bytes, Some(crate::files::THUMBNAIL as u32), INLINE_BUDGET)?
                    .into_iter()
                    .map(frame_image)
                    .collect()
            }
        })
    })
    .await??;
    Ok((frames, keep))
}

fn still(image: image::DynamicImage) -> (egui::ColorImage, u32) {
    let image = image.into_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    (
        egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw()),
        0,
    )
}

/// Download a file and save it here.
pub(super) async fn save(ctx: &Ctx, client: &FileClient, file: BlobRef, path: PathBuf) {
    let id = file.id;
    let saved = async {
        let shown = ctx.clone();
        let progress = Arc::new(move |done: u64, total: u64| {
            shown.send(Update::SaveProgress(id, done as f32 / total.max(1) as f32));
        });
        let bytes = client.download_file(&file, Some(progress)).await?;
        tokio::task::spawn_blocking(move || save_file(&path, &bytes)).await?
    };
    let saved = saved.await.map_err(|error| {
        tracing::warn!(error = %format_args!("{error:#}"), "saving a file failed");
        gone(&error)
    });
    ctx.send(Update::Saved(id, saved));
}

/// The largest file that still fits the server's 32 MiB blob limit with its
/// 16-byte tag per 64 KiB chunk.
pub const MAX_FILE: u64 = 32 * 1024 * 1024 - 8 * 1024;
const _: () = assert!(MAX_FILE + MAX_FILE.div_ceil(64 * 1024) * 16 <= 32 * 1024 * 1024);

/// Encrypt and upload each file, adding it to `out` so a failure can remove the earlier ones.
async fn upload(
    client: &FileClient,
    channel: &Id,
    files: Vec<PathBuf>,
    out: &mut Vec<Attachment>,
    shown: impl Fn(f32) + Clone + Send + Sync + 'static,
) -> Result<()> {
    for (at, path) in files.into_iter().enumerate() {
        let name = file_name(&path);
        let media_type = media_type(&path);
        let bytes = read(path).await?;
        let shown = shown.clone();
        let progress = Arc::new(move |done: u64, total: u64| {
            shown(at as f32 + done as f32 / total.max(1) as f32);
        });
        out.push(
            client
                .upload_file(channel, &name, media_type, &bytes, Some(progress))
                .await?,
        );
    }
    Ok(())
}

/// A file to upload, refusing one bigger than the server's blob limit.
async fn read(path: PathBuf) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let file = fs::File::open(&path)?;
        let size = file.metadata()?.len();
        if size > MAX_FILE {
            return Err(StoredFileTooLarge(size, file_name(&path)).into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE {
            return Err(StoredFileTooLarge(bytes.len() as u64, file_name(&path)).into());
        }
        Ok(bytes)
    })
    .await?
}

/// Load a channel's shared files; when that fails, say so and show none.
pub(super) async fn shared(ctx: &Ctx, url: &Url, client: &mut KeyClient<'_>, channel: Id) {
    let list = client.shared_files(&channel).await.unwrap_or_else(|error| {
        tracing::warn!(error = %format_args!("{error:#}"), "loading shared files failed");
        ctx.send(Update::Failed(t!("files.shared_failed").into()));
        Vec::new()
    });
    ctx.send(Update::SharedFiles(url.clone(), channel, list));
}

/// Upload files into a channel's shared files one at a time, then load the list again.
/// They go without thumbnails: a thumbnail would expire while its file stays.
pub(super) async fn share(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    channel: Id,
    paths: Vec<PathBuf>,
) {
    for path in paths {
        let name = file_name(&path);
        let shown: Progress = {
            let (ctx, url, name) = (ctx.clone(), url.clone(), name.clone());
            Arc::new(move |done: u64, total: u64| {
                let what = Some((name.clone(), done as f32 / total.max(1) as f32));
                ctx.send(Update::Uploading(url.clone(), channel, what));
            })
        };
        shown(0, 1);
        let shared = async {
            let bytes = read(path.clone()).await?;
            let (file, _) = prepare_file(&name, media_type(&path), &bytes, &[])?;
            client
                .upload_prepared(&channel, &file, &bytes, None, Some(shown))
                .await?;
            let mut shared = client.share_file(&channel, &file).await;
            // The channel just got a new key: take it and try once more.
            if code(shared.as_ref().err()) == Some("epoch_changed") && client.sync().await.is_ok() {
                shared = client.share_file(&channel, &file).await;
            }
            if let Err(error) = shared {
                discard(&client.file_client(), std::slice::from_ref(&file)).await;
                return Err(error);
            }
            Ok(())
        };
        if let Err(error) = shared.await {
            tracing::warn!(error = %format_args!("{error:#}"), "sharing a file failed");
            ctx.send(Update::Failed(upload_problem(&error)));
            break;
        }
    }
    ctx.send(Update::Uploading(url.clone(), channel, None));
    shared(ctx, url, client, channel).await;
}

/// Remove a shared file, then load the list again.
pub(super) async fn unshare(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    (channel, id, uploader): (Id, Id, Id),
) {
    if let Err(error) = client.delete_message_file(&id, &uploader).await
        && !gone(&error)
    {
        tracing::warn!(error = %format_args!("{error:#}"), "removing a shared file failed");
        ctx.send(Update::Failed(t!("files.remove_failed").into()));
    }
    shared(ctx, url, client, channel).await;
}

/// Remove uploaded files that will not be sent after all.
async fn discard(client: &FileClient, files: &[Attachment]) {
    for file in files {
        for blob in std::iter::once(&file.file).chain(&file.thumbnail) {
            if let Err(error) = client.delete_file(&blob.id).await {
                tracing::warn!(error = %format_args!("{error:#}"), "removing an unsent file failed");
            }
        }
    }
}

pub fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || t!("files.unnamed").into(),
        |name| fit_file_name(&name.to_string_lossy()),
    )
}

/// Only a hint for the receiver; images are recognised by their contents.
fn media_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// The server answered 409, as for a message sent in an epoch that just ended.
pub(super) fn conflict(error: &anyhow::Error) -> bool {
    status(error) == Some(StatusCode::CONFLICT)
}

fn status(error: &anyhow::Error) -> Option<StatusCode> {
    error
        .downcast_ref::<UploadRejected>()
        .map(|rejected| rejected.status)
        .or_else(|| {
            error
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status)
        })
}

/// The server no longer has this file: it expired or was deleted.
pub(super) fn gone(error: &anyhow::Error) -> bool {
    status(error) == Some(StatusCode::NOT_FOUND)
}

/// The API's error code for a refused upload or share.
fn code(error: Option<&anyhow::Error>) -> Option<&str> {
    error?
        .downcast_ref::<UploadRejected>()
        .map(|r| r.code.as_str())
}

#[derive(Debug)]
struct StoredFileTooLarge(u64, String);

impl std::fmt::Display for StoredFileTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stored file exceeds client limit ({} bytes)", self.0)
    }
}

impl std::error::Error for StoredFileTooLarge {}

fn upload_problem(error: &anyhow::Error) -> String {
    if let Some(StoredFileTooLarge(size, name)) = error.downcast_ref::<StoredFileTooLarge>() {
        return crate::files::too_big(name, *size);
    }
    if error.downcast_ref::<MessageError>() == Some(&MessageError::ServerUpdateRequired) {
        return t!("net.rotation_update").into();
    }
    match (status(error), code(Some(error))) {
        (_, Some("shared_limit")) => t!("files.err_shared_limit"),
        (_, Some("file_too_large")) => t!("files.err_too_large"),
        (_, Some("file_member_quota")) => t!("files.err_member_quota"),
        (_, Some("file_server_quota")) => t!("files.err_server_quota"),
        (Some(StatusCode::PAYLOAD_TOO_LARGE), _) => {
            t!("files.err_no_room")
        }
        (Some(StatusCode::SERVICE_UNAVAILABLE), _) => {
            t!("files.err_busy")
        }
        (Some(StatusCode::FORBIDDEN), _) => t!("files.err_forbidden"),
        _ => t!("files.err_failed"),
    }
    .into()
}

/// Write next to the target first, so a failed download leaves no partial file.
fn save_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let folder = path.parent().context("no folder to save in")?;
    let mut file = tempfile::NamedTempFile::new_in(folder)?;
    file.write_all(bytes)?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared() -> Option<(Attachment, Option<Vec<u8>>)> {
        let (attachment, thumbnail) = prepare_file(FILE_NAME, MEDIA_TYPE, b"{}", b"").unwrap();
        Some((attachment, thumbnail))
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn downloads_run_a_few_at_a_time_off_the_loop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (running, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut jobs = Jobs::default();
        for _ in 0..DOWNLOADS * 3 {
            let (running, most) = (running.clone(), most.clone());
            let again = Download::LinkCard(BlobRef::for_tests([1; 32]));
            jobs.download(again, async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                None
            });
        }
        let mut originals = ImageOriginals::default();
        while jobs.busy() {
            let next = jobs.next(&mut originals).await;
            assert!(next.is_none(), "a download hands nothing back");
        }
        assert_eq!(most.load(Ordering::SeqCst), DOWNLOADS);
        assert!(
            jobs.stop().is_empty(),
            "finished downloads aren't asked again"
        );
    }

    fn draft(channel: u8, text: &str) -> Draft {
        Draft {
            forum_post: false,
            id: [0; 32],
            channel: [channel; 32],
            peer: None,
            text: text.into(),
            reply_to: None,
            edit_of: None,
            revision: 0,
            files: Vec::new(),
            keep: Vec::new(),
            sticker: None,
            friend: None,
            poke: false,
            poll: None,
            voice_request: None,
            preview: None,
            inline: Vec::new(),
            queued: Queued::default(),
        }
    }

    fn target(draft: &Draft) -> Target {
        (draft.channel, draft.id, false, false)
    }

    fn texts(commands: &[Command]) -> Vec<&str> {
        (commands.iter())
            .map(|command| match command {
                Command::Send(draft) => draft.text.as_str(),
                _ => "download",
            })
            .collect()
    }

    /// Text, picture, text in one channel: the second text waits for the picture.
    #[tokio::test]
    async fn a_message_sent_after_an_upload_waits_for_it() {
        let mut jobs = Jobs::default();
        let mut originals = ImageOriginals::default();
        assert!(
            jobs.wait(draft(1, "before")).is_some(),
            "nothing to wait for"
        );
        let (done, uploaded) = tokio::sync::oneshot::channel::<()>();
        let picture = draft(1, "picture");
        jobs.upload(
            picture.clone(),
            Queued::default(),
            target(&picture),
            async move {
                uploaded.await.unwrap();
                (MessageBody::default(), Some(Vec::new()))
            },
        );
        assert!(jobs.wait(draft(1, "after")).is_none(), "same channel waits");
        assert!(jobs.wait(draft(1, "later")).is_none());
        let other = jobs.wait(draft(2, "elsewhere"));
        assert_eq!(other.map(|d| d.text).as_deref(), Some("elsewhere"));
        done.send(()).unwrap();
        let finished = jobs.next(&mut originals).await.expect("the upload is back");
        assert_eq!(finished.draft.text, "picture");
        assert!(finished.upload.is_some());
        let waiting: Vec<_> = finished.waiting.iter().map(|d| d.text.as_str()).collect();
        assert_eq!(waiting, ["after", "later"], "in the order they were sent");
        assert!(
            jobs.wait(draft(1, "now")).is_some(),
            "the channel is free again"
        );
        assert!(!jobs.busy());
    }

    #[tokio::test]
    async fn a_failed_upload_gives_its_messages_back() {
        let mut jobs = Jobs::default();
        let picture = draft(7, "picture");
        jobs.upload(
            picture.clone(),
            Queued::default(),
            target(&picture),
            async { panic!("synthetic upload failure") },
        );
        assert!(jobs.wait(draft(7, "after")).is_none());
        let finished =
            (jobs.next(&mut ImageOriginals::default()).await).expect("the lane comes back");
        assert_eq!(finished.draft.text, "picture");
        assert!(finished.upload.is_none());
        assert_eq!(finished.waiting.len(), 1);
        assert!(!jobs.busy());
    }

    /// A dropped connection stops the transfers; they are asked again, in order.
    #[tokio::test]
    async fn stopping_hands_back_every_transfer_in_order() {
        let mut jobs = Jobs::default();
        let picture = draft(3, "picture");
        jobs.upload(
            picture.clone(),
            Queued::default(),
            target(&picture),
            std::future::pending(),
        );
        assert!(jobs.wait(draft(3, "after")).is_none());
        assert!(jobs.wait(draft(3, "later")).is_none());
        let again = Download::Save(BlobRef::for_tests([2; 32]), PathBuf::from("x"));
        jobs.download(again, std::future::pending());
        let again = jobs.stop();
        assert_eq!(texts(&again), ["download", "picture", "after", "later"]);
        assert!(!jobs.busy(), "the tasks are gone");
        assert!(
            jobs.wait(draft(3, "next")).is_some(),
            "nothing waits any more"
        );
    }

    #[test]
    fn clearing_originals_stops_a_running_download_from_keeping_one() {
        let mut originals = ImageOriginals::default();
        let file = BlobRef::for_tests([1; 32]);
        let asked = originals.lookup(&file, Fetch::Inline);
        originals.clear();
        originals.remember((
            asked.generation,
            file.clone(),
            Arc::new(Zeroizing::new(vec![0])),
        ));
        assert!(originals.lookup(&file, Fetch::Full).bytes.is_none());
    }

    #[tokio::test]
    async fn animated_original_is_downloaded_once_for_inline_and_full() {
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            for (color, millis) in [([255, 0, 0, 255], 60), ([0, 255, 0, 255], 120)] {
                let frame = image::RgbaImage::from_pixel(640, 280, image::Rgba(color));
                encoder
                    .encode_frame(image::Frame::from_parts(
                        frame,
                        0,
                        0,
                        image::Delay::from_numer_denom_ms(millis, 1),
                    ))
                    .unwrap();
            }
        }
        let (file, _) = prepare_file("Synthetic.gif", "image/gif", &gif, &[]).unwrap();
        let downloads = std::cell::Cell::new(0);
        let download = || async {
            downloads.set(downloads.get() + 1);
            Ok(gif.clone())
        };
        let mut originals = ImageOriginals::default();
        let asked = originals.lookup(&file.file, Fetch::Inline);
        let (inline, keep) = image_frames(asked.bytes, Fetch::Inline, download())
            .await
            .unwrap();
        originals.remember((asked.generation, file.file.clone(), keep.expect("kept")));
        assert_eq!(downloads.get(), 1);
        let cached = originals.lookup(&file.file, Fetch::Full).bytes;
        let (full, keep) = image_frames(cached, Fetch::Full, download()).await.unwrap();
        assert!(keep.is_none(), "kept once");
        assert_eq!(downloads.get(), 1, "the original was already authenticated");
        assert_eq!(inline.len(), 2);
        assert_eq!(full.len(), 2);
        for ((inline, inline_ms), (original, original_ms)) in inline.iter().zip(&full) {
            assert_eq!(inline.size, [320, 140]);
            assert_eq!(original.size, [640, 280]);
            assert_eq!(inline_ms, original_ms);
        }
        assert_eq!([full[0].1, full[1].1], [60, 120]);
        eprintln!(
            "inline_downloads=1 full_upgrade_downloads=0 inline_decodes=1 full_decodes=1 frames_per_decode=2"
        );

        let mut other = file.file.clone();
        other.size += 1;
        assert!(
            originals.lookup(&other, Fetch::Full).bytes.is_none(),
            "matching the ID alone does not authenticate another reference"
        );
        assert!(
            originals
                .lookup(&file.file, Fetch::Thumbnail)
                .bytes
                .is_none(),
            "a thumbnail is its own file"
        );
        originals.clear();
        assert!(
            originals.lookup(&file.file, Fetch::Full).bytes.is_none(),
            "discarded originals require a new fetch"
        );
    }

    #[test]
    fn a_stalled_preview_upload_and_cleanup_end_on_time() {
        let started = Instant::now();
        let deadline = started + std::time::Duration::from_millis(200);
        let result = runtime().block_on(async {
            let result = within(deadline, async { prepared() }, |_, _| {
                std::future::pending::<Result<()>>()
            })
            .await;
            // The DELETEs stall too; removing gives up on its own limit.
            at_most(
                std::time::Duration::from_millis(100),
                std::future::pending(),
            )
            .await;
            result
        });
        let elapsed = started.elapsed();
        assert!(
            matches!(result, Preview::Left(Some(_))),
            "the prepared IDs are kept for removal"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(1000),
            "{elapsed:?}"
        );
        let stalled_prepare = runtime().block_on(within(
            Instant::now() + std::time::Duration::from_millis(50),
            std::future::pending(),
            |_, _| async { Ok(()) },
        ));
        assert!(
            matches!(stalled_prepare, Preview::Left(None)),
            "preparing counts toward the deadline"
        );
    }

    #[test]
    fn a_stored_preview_whose_answer_was_lost_is_removed() {
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let result = runtime().block_on(within(deadline, async { prepared() }, |_, _| async {
            anyhow::bail!("connection closed before the answer")
        }));
        let Preview::Left(Some(attachment)) = result else {
            panic!("the prepared file must come back for removal");
        };
        assert_eq!(attachment.media_type, MEDIA_TYPE);
        let ok = runtime().block_on(within(deadline, async { prepared() }, |_, _| async {
            Ok(())
        }));
        assert!(matches!(ok, Preview::Sent(_)));
    }

    #[test]
    fn a_late_or_oversized_preview_is_left_out() {
        let made = |deadline, url: String| Made {
            card: Card {
                url,
                title: "t".into(),
                description: String::new(),
                site: String::new(),
            },
            picture: None,
            deadline,
        };
        let later = Instant::now() + std::time::Duration::from_secs(5);
        assert!(preview_file(&made(later, "https://example.org/".into())).is_some());
        assert!(preview_file(&made(Instant::now(), "https://example.org/".into())).is_none());
        let long = format!("https://example.org/{}", "a".repeat(5000));
        assert!(preview_file(&made(later, long)).is_none());
    }

    #[tokio::test]
    async fn stored_file_size_failure_names_size_and_client_limit() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(177 * crate::files::MIB).unwrap();
        let error = read(file.path().to_owned()).await.unwrap_err();
        assert_eq!(
            upload_problem(&error),
            crate::files::too_big(&file_name(file.path()), 177 * crate::files::MIB)
        );
        file.as_file().set_len(MAX_FILE).unwrap();
        assert_eq!(
            read(file.path().to_owned()).await.unwrap().len() as u64,
            MAX_FILE
        );
        file.as_file().set_len(MAX_FILE + 1).unwrap();
        assert!(
            read(file.path().to_owned())
                .await
                .unwrap_err()
                .is::<StoredFileTooLarge>()
        );
        std::fs::write(file.path(), b"synthetic control").unwrap();
        assert_eq!(
            read(file.path().to_owned()).await.unwrap(),
            b"synthetic control"
        );
    }

    #[test]
    fn upload_problems_name_the_limit_that_was_hit() {
        let rejected = |status, code: &str| {
            anyhow::Error::from(UploadRejected {
                status,
                code: code.into(),
            })
        };
        let full = StatusCode::PAYLOAD_TOO_LARGE;
        assert_eq!(
            upload_problem(&rejected(full, "file_too_large")),
            "This file exceeds this server's upload limit."
        );
        assert_eq!(
            upload_problem(&rejected(full, "file_member_quota")),
            "You reached your storage limit."
        );
        assert_eq!(
            upload_problem(&rejected(full, "file_server_quota")),
            "This server is full."
        );
        assert_eq!(
            upload_problem(&rejected(
                StatusCode::SERVICE_UNAVAILABLE,
                "file_upload_busy"
            )),
            "Too many uploads right now. Try again in a moment."
        );
    }
}
