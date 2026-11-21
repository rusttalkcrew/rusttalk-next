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
        Attachment, BlobRef, KeyClient, MessageBody, UploadRejected, decode_frames,
        decode_thumbnail,
    },
    messages::fit_file_name,
};
use url::Url;

use super::{Ctx, Fetch, Update, directory::frame_image, send_messages, to_msg};
use crate::server::Id;

/// Upload the files, then send the message with them; `false` when an upload failed.
pub(super) async fn send(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    (channel, id): (Id, Id),
    mut body: MessageBody,
    files: Vec<PathBuf>,
) -> Result<bool> {
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
            discard(client, &body.attachments).await;
            ctx.send(Update::Failed(upload_problem(&error).into()));
            return Ok(false);
        }
    }
    let record = match client.post_message(&channel, id, &body).await {
        Ok(record) => record,
        Err(error) => {
            discard(client, &body.attachments).await;
            return Err(error);
        }
    };
    send_messages(ctx, url, channel, vec![to_msg(&record, &body)], false, None);
    Ok(true)
}

/// Delete a message, then its stored files.
pub(super) async fn delete(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
    (channel, id, sender): (Id, Id, Id),
    files: Vec<Id>,
) -> Result<()> {
    match client.delete_message(&channel, &id).await {
        // Already gone there counts as deleted, so a DM delete can finish.
        Err(error) if !gone(&error) => return Err(error),
        _ => ctx.send(Update::Deleted(url.clone(), channel, id)),
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
        let bytes = tokio::task::spawn_blocking(move || {
            let mut bytes = Vec::new();
            fs::File::open(&path)?
                .take(MAX_FILE + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(bytes.len() as u64 <= MAX_FILE, "file too large");
            Ok(bytes)
        })
        .await??;
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

fn upload_problem(error: &anyhow::Error) -> &'static str {
    let code = error
        .downcast_ref::<UploadRejected>()
        .map(|r| r.code.as_str());
    match (status(error), code) {
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
            "File too large."
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
