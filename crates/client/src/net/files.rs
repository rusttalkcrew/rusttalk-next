//! Stored files: upload, download, save, and plain words for what went wrong.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use eframe::egui;
use reqwest::StatusCode;
use rusttalk_core::{
    client::{
        Attachment, BlobRef, KeyClient, MessageBody, UploadRejected, decode_image, decode_thumbnail,
    },
    messages::fit_file_name,
};
use url::Url;

use super::{Ctx, Update, send_messages, to_msg};
use crate::server::Id;

/// Upload the files, then send the message with them.
pub(super) async fn send(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    channel: Id,
    mut body: MessageBody,
    files: Vec<PathBuf>,
) -> Result<()> {
    if let Some(first) = files.first() {
        let label = match files.len() {
            1 => file_name(first),
            count => format!("{count} files"),
        };
        // shortcut: uploads run inside this server's loop, so its events wait until they
        // finish (at most 4 × 32 MiB); give them their own task if that gets noticed.
        ctx.send(Update::Uploading(url.clone(), channel, Some(label)));
        let uploaded = upload(client, &channel, files, &mut body.attachments).await;
        ctx.send(Update::Uploading(url.clone(), channel, None));
        if let Err(error) = uploaded {
            tracing::warn!(error = %format_args!("{error:#}"), "uploading a file failed");
            discard(client, &body.attachments).await;
            ctx.send(Update::Failed(upload_problem(&error).into()));
            return Ok(());
        }
    }
    let record = match client.send_message(&channel, &body).await {
        Ok(record) => record,
        Err(error) => {
            discard(client, &body.attachments).await;
            return Err(error);
        }
    };
    send_messages(ctx, url, channel, vec![to_msg(&record, &body)], false, None);
    Ok(())
}

/// Delete a message, then its stored files.
pub(super) async fn delete(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
    (channel, id, sender): (Id, Id, Id),
    files: Vec<Id>,
) -> Result<()> {
    client.delete_message(&channel, &id).await?;
    ctx.send(Update::Deleted(url.clone(), channel, id));
    for file in files {
        if let Err(error) = client.delete_message_file(&file, &sender).await
            && !gone(&error)
        {
            tracing::warn!(error = %format_args!("{error:#}"), "deleting a stored file failed");
        }
    }
    Ok(())
}

/// Download and decode a picture: a thumbnail, or the full image (`true`).
pub(super) async fn image(ctx: &Ctx, client: &KeyClient<'_>, file: BlobRef, full: bool) {
    let id = file.id;
    let image = async {
        let bytes = client.download_file(&file).await?;
        // Received pictures are hostile: decode with limits, off the async workers.
        tokio::task::spawn_blocking(move || {
            let image = if full {
                decode_image(&bytes)
            } else {
                decode_thumbnail(&bytes)
            }?
            .into_rgba8();
            let size = [image.width() as usize, image.height() as usize];
            anyhow::Ok(egui::ColorImage::from_rgba_unmultiplied(
                size,
                image.as_raw(),
            ))
        })
        .await?
    };
    let image = image.await.map_err(|error| {
        tracing::warn!(error = %format_args!("{error:#}"), "loading a picture failed");
        gone(&error)
    });
    ctx.send(Update::Image(id, image));
}

/// Download a file and save it here.
pub(super) async fn save(ctx: &Ctx, client: &KeyClient<'_>, file: BlobRef, path: PathBuf) {
    let id = file.id;
    let saved = async {
        let bytes = client.download_file(&file).await?;
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
) -> Result<()> {
    for path in files {
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
        out.push(
            client
                .upload_file(channel, &name, media_type, &bytes)
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
        || "file".into(),
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
fn gone(error: &anyhow::Error) -> bool {
    status(error) == Some(StatusCode::NOT_FOUND)
}

fn upload_problem(error: &anyhow::Error) -> &'static str {
    let code = error
        .downcast_ref::<UploadRejected>()
        .map(|r| r.code.as_str());
    match (status(error), code) {
        (_, Some("file_too_large")) => "File too large.",
        (_, Some("file_member_quota")) => "You reached your storage limit.",
        (_, Some("file_server_quota")) => "This server is full.",
        (Some(StatusCode::PAYLOAD_TOO_LARGE), _) => {
            "There's no room for this file. You or this server reached the storage limit."
        }
        (Some(StatusCode::SERVICE_UNAVAILABLE), _) => {
            "Too many uploads right now. Try again in a moment."
        }
        (Some(StatusCode::FORBIDDEN), _) => "You can't send files here.",
        _ => "Couldn't send the file. Try again.",
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
