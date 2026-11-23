//! Stored files: upload, download, save, and plain words for what went wrong.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use eframe::egui;
use reqwest::StatusCode;
use rusttalk_core::{
    client::{
        Attachment, BlobRef, KeyClient, MessageBody, MessageError, Progress, UploadRejected,
        decode_frames, decode_thumbnail,
    },
    messages::fit_file_name,
};
use url::Url;

use sha2::Digest as _;

use super::{Ctx, Fetch, State, Update, directory::frame_image};
use crate::{
    link_cards::{Card, FILE_NAME, MAX_CARD_BYTES, MEDIA_TYPE, Made},
    server::Id,
    sticker_files::{Inline, Kind},
};
use rusttalk_core::client::prepare_file;
use std::time::Instant;

/// Upload the files, then send the message with them; `false` when an upload failed.
/// A link preview that fails to upload is left out, without failing the message.
/// Files already in `body` are kept ones (see `Draft::keep`): never removed here.
pub(super) async fn send(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    (channel, id, page, forum_post): (Id, Id, bool, bool),
    mut body: MessageBody,
    (files, preview, inline): (Vec<PathBuf>, Option<Arc<Made>>, Vec<Inline>),
) -> Result<bool> {
    let kept = body.attachments.len();
    if let Some(first) = files.first() {
        let label = match files.len() {
            1 => file_name(first),
            count => t!("files.count", count = count),
        };
        // shortcut: uploads run inside this server's loop, so its events wait until they
        // finish (at most 4 × 32 MiB); give them their own task if that gets noticed.
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
            return Ok(false);
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
                return Ok(false);
            }
        }
    }
    let posted = match page {
        true => client.post_page(&channel, id, &body).await,
        false => client.post_message(&channel, id, &body).await,
    };
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
                    at_most(CLEANUP, discard(client, &leftover)).await;
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
            discard(client, &body.attachments[kept..]).await;
            at_most(CLEANUP, discard(client, &leftover)).await;
            return Err(error);
        }
    };
    state.sent(ctx, url, channel, &record, &posted_body);
    at_most(CLEANUP, discard(client, &leftover)).await;
    Ok(true)
}

/// Upload another server's emoji or sticker as a small file (no thumbnail) on this one.
async fn upload_inline(
    client: &KeyClient<'_>,
    channel: &Id,
    picture: &Inline,
) -> Result<Attachment> {
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
async fn upload_preview(client: &KeyClient<'_>, channel: &Id, made: Arc<Made>) -> Preview {
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
pub(super) async fn link_card(ctx: &Ctx, client: &KeyClient<'_>, file: BlobRef) {
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
pub(super) async fn sticker_file(ctx: &Ctx, client: &KeyClient<'_>, file: BlobRef, kind: Kind) {
    let id = file.id;
    let picture = async {
        anyhow::ensure!(
            file.size as usize <= rusttalk_core::keys::IMAGE_PLAINTEXT,
            "emoji file too big"
        );
        let bytes = client.download_file(&file, None).await?;
        tokio::task::spawn_blocking(move || {
            let frames = crate::sticker_files::decode(&bytes, kind)?;
            Ok(crate::server::Picture {
                key: sha2::Sha256::digest(&bytes).into(),
                frames: Arc::new(frames.into_iter().map(frame_image).collect()),
                bytes: Arc::new(bytes),
            })
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

/// Download and decode a picture as asked.
pub(super) async fn image(ctx: &Ctx, client: &KeyClient<'_>, file: BlobRef, fetch: Fetch) {
    let id = file.id;
    let frames = async {
        let bytes = client.download_file(&file, None).await?;
        // Received pictures are hostile: decode with limits, off the async workers.
        tokio::task::spawn_blocking(move || {
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
        .await?
    };
    let frames = frames.await.map_err(|error: anyhow::Error| {
        tracing::warn!(error = %format_args!("{error:#}"), "loading a picture failed");
        gone(&error)
    });
    ctx.send(Update::Image(id, fetch, frames));
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
pub(super) async fn save(ctx: &Ctx, client: &KeyClient<'_>, file: BlobRef, path: PathBuf) {
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
    client: &KeyClient<'_>,
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
                discard(client, std::slice::from_ref(&file)).await;
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
async fn discard(client: &KeyClient<'_>, files: &[Attachment]) {
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
