use std::{
    collections::HashMap,
    env, io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    body::{Body, BodyDataStream},
    extract::{
        Path as RoutePath, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use rusttalk_core::{
    auth::{FileLimits, decode, decode_bytes},
    messages::{MAX_SHARED_ENVELOPE, MessageRecord, Purpose},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{FromRow, SqliteConnection, SqlitePool, query, query_as, query_scalar};
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::{Semaphore, SemaphorePermit},
    time,
};
use tokio_util::io::ReaderStream;
use tracing::Instrument;

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    file_settings, messages,
    roles::{self, Permission},
    store,
};

pub struct Files {
    dir: PathBuf,
    pub(crate) ceilings: FileLimits,
    uploads: Semaphore,
    uploaders: Mutex<HashMap<[u8; 32], usize>>,
}

/// An upload fails when no bytes arrive for this long, so slow links still finish.
const UPLOAD_IDLE: Duration = Duration::from_secs(30);
/// The longest an upload may hold a staging slot.
const UPLOAD_LIMIT: Duration = Duration::from_secs(15 * 60);

/// Uploads one member may stage at once, so a single slow sender cannot hold every slot.
const UPLOADS_PER_MEMBER: usize = 2;

struct UploadSlot<'a> {
    files: &'a Files,
    person: [u8; 32],
    _permit: SemaphorePermit<'a>,
}

impl Drop for UploadSlot<'_> {
    fn drop(&mut self) {
        let mut uploaders = self
            .files
            .uploaders
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = uploaders.get_mut(&self.person) {
            *count -= 1;
            if *count == 0 {
                uploaders.remove(&self.person);
            }
        }
    }
}

impl Files {
    pub async fn open(data: &Path, db: &SqlitePool) -> Result<Self> {
        let files = Self {
            dir: data.join("files"),
            ceilings: file_settings::ceilings()?,
            uploads: Semaphore::new(4),
            uploaders: Mutex::default(),
        };
        fs::create_dir_all(&files.dir).await?;
        files.remove_orphans(db).await?;
        Ok(files)
    }

    fn upload_slot(&self, person: [u8; 32]) -> ApiResult<UploadSlot<'_>> {
        let busy = || ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "file_upload_busy");
        let mut uploaders = self.uploaders.lock().unwrap_or_else(|e| e.into_inner());
        if uploaders.get(&person).copied().unwrap_or(0) >= UPLOADS_PER_MEMBER {
            return Err(busy());
        }
        let permit = self.uploads.try_acquire().map_err(|_| busy())?;
        *uploaders.entry(person).or_default() += 1;
        Ok(UploadSlot {
            files: self,
            person,
            _permit: permit,
        })
    }

    async fn remove_orphans(&self, db: &SqlitePool) -> Result<()> {
        // An interruption before the SQLite commit can leave ciphertext without a row.
        let mut entries = fs::read_dir(&self.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(id) = name.to_str() else { continue };
            if id.starts_with(".upload-")
                || (decode::<32>(id).is_ok()
                    && !query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM files WHERE id = ?)")
                        .bind(id)
                        .fetch_one(db)
                        .await?)
            {
                fs::remove_file(entry.path()).await?;
            }
        }
        Ok(())
    }
}

pub(crate) fn setting(name: &str, default: i64) -> Result<i64> {
    let value = match env::var(name) {
        Ok(value) => value
            .parse::<i64>()
            .with_context(|| format!("invalid {name}"))?,
        Err(env::VarError::NotPresent) => default,
        Err(error) => return Err(error).with_context(|| format!("invalid {name}")),
    };
    ensure!(value > 0, "{name} must be positive");
    Ok(value)
}

#[derive(Deserialize)]
pub struct Upload {
    channel: String,
    id: String,
}

pub async fn upload(
    State(app): State<App>,
    headers: HeaderMap,
    args: Result<Query<Upload>, QueryRejection>,
    body: Body,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Query(Upload { channel, id }) = args.map_err(|_| ApiError::bad_request("invalid_query"))?;
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    messages::require_write(&mut tx, &actor, &channel).await?;
    let files = &app.files;
    let max_file = file_settings::effective(&mut tx, files.ceilings)
        .await?
        .max_file_bytes;
    tx.commit().await?;

    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|h| h.to_str().ok())
        .and_then(|n| n.parse::<u64>().ok())
        .is_some_and(|n| n > max_file as u64)
    {
        drain(&mut body.into_data_stream()).await;
        return Err(too_large());
    }
    let _slot = files.upload_slot(actor.person)?;
    let temp = tempfile::Builder::new()
        .prefix(".upload-")
        .tempfile_in(&files.dir)
        .map_err(disk_error)?;
    let (output, temp) = temp.into_parts();
    let mut output = fs::File::from_std(output);
    let timeout = || ApiError::new(StatusCode::REQUEST_TIMEOUT, "file_upload_timeout");
    let size = time::timeout(UPLOAD_LIMIT, async {
        let mut body = body.into_data_stream();
        let mut size = 0;
        let mut oversized = false;
        while let Some(chunk) = time::timeout(UPLOAD_IDLE, body.try_next())
            .await
            .map_err(|_| timeout())?
            .map_err(|_| ApiError::bad_request("invalid_file_body"))?
        {
            // Keep reading an oversized body to its end: answering while the client still
            // sends makes the proxy see a reset and report 502 instead of 413.
            if oversized || chunk.len() as i64 > max_file - size {
                oversized = true;
                continue;
            }
            size += chunk.len() as i64;
            output.write_all(&chunk).await.map_err(disk_error)?;
        }
        if oversized {
            return Err(too_large());
        }
        Ok(size)
    })
    .await
    .map_err(|_| timeout())??;
    if size < 16 {
        return Err(ApiError::bad_request("invalid_file_body"));
    }
    output.sync_all().await.map_err(disk_error)?;
    drop(output);

    // Reauthorize and include quota checks, ID reservation and file installation in one write transaction.
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    messages::require_write(&mut tx, &actor, &channel).await?;
    let limits = file_settings::effective(&mut tx, files.ceilings).await?;
    if size > limits.max_file_bytes {
        return Err(too_large());
    }
    let used: i64 = query_scalar("SELECT COALESCE(SUM(size), 0) FROM files WHERE uploader = ?")
        .bind(actor.person.as_slice())
        .fetch_one(&mut *tx)
        .await?;
    if size > limits.member_storage_bytes - used {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_member_quota",
        ));
    }
    let used = file_settings::used_bytes(&mut tx).await?;
    if size > limits.server_storage_bytes - used {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_server_quota",
        ));
    }
    let expires = actor.at.saturating_add(limits.retention_secs);
    // An ID is never reused, even after its file expired or was deleted.
    let reserved = query("INSERT INTO file_ids (id) VALUES (?) ON CONFLICT (id) DO NOTHING")
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if reserved == 0 {
        return Err(ApiError::new(StatusCode::CONFLICT, "file_exists"));
    }
    query(
        "INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&channel)
    .bind(actor.person.as_slice())
    .bind(size)
    .bind(actor.at)
    .bind(expires)
    .execute(&mut *tx)
    .await?;
    let path = files.dir.join(&id);
    temp.persist_noclobber(&path)
        .map_err(|error| disk_error(error.error))?;
    // Once installed, finish the disk and database change even if the HTTP client leaves.
    let files = app.files.clone();
    tokio::spawn(
        async move {
            let committed = async {
                files.sync_dir().await?;
                tx.commit().await?;
                Ok::<_, ApiError>(())
            }
            .await;
            if let Err(error) = committed {
                if let Err(cleanup) = fs::remove_file(path).await {
                    tracing::error!(kind = ?cleanup.kind(), "uncommitted file cleanup failed");
                }
                return Err(error);
            }
            tracing::info!(id, size, "file uploaded");
            Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
        }
        .in_current_span(),
    )
    .await
    .map_err(|_| ApiError::internal("file_task_failed"))?
}

#[derive(FromRow)]
struct Stored {
    channel_id: String,
    uploader: Vec<u8>,
    size: i64,
}

async fn lookup(db: &mut SqliteConnection, id: &str, at: i64) -> ApiResult<Stored> {
    query_as("SELECT channel_id, uploader, size FROM files WHERE id = ? AND expires_at > ?")
        .bind(id)
        .bind(at)
        .fetch_optional(db)
        .await?
        .ok_or_else(missing)
}

fn missing() -> ApiError {
    ApiError::not_found("file_not_found")
}

fn disk_error(error: io::Error) -> ApiError {
    tracing::warn!(kind = ?error.kind(), "file storage I/O failed");
    ApiError::internal("file_storage_error")
}

/// Reads and discards the rest of a rejected body within the upload timeouts, so the
/// rejection reaches the client instead of a proxy error for a reset stream.
pub(crate) async fn drain(body: &mut BodyDataStream) {
    let rest =
        async { while let Ok(Ok(Some(_))) = time::timeout(UPLOAD_IDLE, body.try_next()).await {} };
    let _ = time::timeout(UPLOAD_LIMIT, rest).await;
}

fn too_large() -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "file_too_large")
}

pub async fn download(
    State(app): State<App>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> ApiResult<Response> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate_read(&app, &headers).await.map_err(|error| {
        if error.status == StatusCode::UNAUTHORIZED {
            missing()
        } else {
            error
        }
    })?;
    let row = lookup(&mut tx, &id, actor.at).await?;
    messages::chat_kind(&mut tx, &actor, &row.channel_id).await?;
    let blob = fs::File::open(app.files.dir.join(&id))
        .await
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                missing()
            } else {
                disk_error(error)
            }
        })?;
    tx.commit().await?;
    tracing::info!(id, size = row.size, "file downloaded");
    let stream = ReaderStream::new(blob).map_err(disk_error);
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, row.size.to_string()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct Remove {
    uploader: Option<String>,
}

pub async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
    args: Result<Query<Remove>, QueryRejection>,
) -> ApiResult<StatusCode> {
    let Query(Remove { uploader }) = args.map_err(|_| ApiError::bad_request("invalid_query"))?;
    decode::<32>(&id)?;
    let uploader = uploader.as_deref().map(decode::<32>).transpose()?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let row = lookup(&mut tx, &id, actor.at).await?;
    let moderated = row.uploader != actor.person;
    let mut dm = None;
    if moderated {
        // A moderator removes files of the message being deleted, named by its sender, so a
        // message that references someone else's file cannot get that file removed.
        if uploader.is_none_or(|person| row.uploader != person) {
            return Err(missing());
        }
        if messages::chat_kind(&mut tx, &actor, &row.channel_id).await? == "dm" {
            dm = Some(row.channel_id.clone());
        }
        roles::authority(&mut tx, &actor.person)
            .await?
            .require(Permission::DeleteMessages)?;
    }
    tokio::spawn(
        async move {
            app.files.remove(&mut tx, &id).await?;
            // Moderator deletes are audited like message deletes; the audit holds ids only.
            if moderated {
                let audit_id = store::audit(
                    &mut tx,
                    &actor.person,
                    "file_deleted",
                    Some(&row.uploader),
                    Some(&id),
                    actor.at,
                )
                .await?;
                if let Some(channel) = dm {
                    query("UPDATE audit_log SET dm_channel_id = ? WHERE id = ?")
                        .bind(channel)
                        .bind(audit_id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            tx.commit().await?;
            tracing::info!(id, size = row.size, "file deleted");
            Ok(StatusCode::NO_CONTENT)
        }
        .in_current_span(),
    )
    .await
    .map_err(|_| ApiError::internal("file_task_failed"))?
}

/// Shared files one channel may keep. The list returns them all.
const SHARED_LIMIT: i64 = 500;
/// JSON for the largest shared-file envelope plus 512 bytes for the ID and epoch.
pub const SHARE_BODY_LIMIT: usize = MAX_SHARED_ENVELOPE.div_ceil(3) * 4 + 512;

#[derive(Deserialize)]
pub struct Share {
    id: String,
    epoch: i64,
    envelope: String,
}

/// Keep the caller's uploaded file in its channel's file list until it is removed. The
/// envelope is a signed message under the file's ID that carries its reference.
pub async fn share(
    State(app): State<App>,
    headers: HeaderMap,
    RoutePath(channel): RoutePath<String>,
    body: Result<Json<Share>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Share {
        id,
        epoch,
        envelope,
    } = crate::payload(body)?;
    decode::<32>(&channel)?;
    let file_id = decode(&id)?;
    let envelope = decode_bytes(&envelope)?;
    if envelope.len() > MAX_SHARED_ENVELOPE {
        return Err(ApiError::bad_request("invalid_message"));
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    messages::require_write(&mut tx, &actor, &channel).await?;
    let row = lookup(&mut tx, &id, actor.at).await?;
    if row.channel_id != channel || row.uploader != actor.person {
        return Err(missing());
    }
    // Shared files never expire, so their count per channel is bounded instead.
    let count: i64 = query_scalar(
        "SELECT COUNT(*) FROM shared_files h JOIN files f ON f.id = h.id WHERE f.channel_id = ?",
    )
    .bind(&channel)
    .fetch_one(&mut *tx)
    .await?;
    if count >= SHARED_LIMIT {
        return Err(ApiError::new(StatusCode::CONFLICT, "shared_limit"));
    }
    let device = messages::verify_signed(
        (Purpose::SharedFile, file_id),
        &app,
        &mut tx,
        &actor,
        &channel,
        epoch,
        &envelope,
    )
    .await?;
    let shared = query(
        "INSERT INTO shared_files (id, epoch, sender_device, sender_person, at, envelope)
         VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
    )
    .bind(&id)
    .bind(epoch)
    .bind(device.as_slice())
    .bind(actor.person.as_slice())
    .bind(actor.at)
    .bind(envelope)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if shared == 0 {
        return Err(ApiError::new(StatusCode::CONFLICT, "file_shared"));
    }
    query("UPDATE files SET expires_at = ? WHERE id = ?")
        .bind(i64::MAX)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(id, size = row.size, "file shared");
    Ok(StatusCode::NO_CONTENT)
}

/// Give a person's shared files (only those from `device`, when given) normal expiry
/// again: they left, were removed, or that device was revoked, so readers no longer
/// trust or list them.
pub(crate) async fn unshare_from(
    app: &App,
    db: &mut SqliteConnection,
    person: &[u8],
    device: Option<&[u8]>,
) -> ApiResult<()> {
    let retention = file_settings::effective(&mut *db, app.files.ceilings)
        .await?
        .retention_secs;
    let ids: Vec<String> = query_scalar(
        "DELETE FROM shared_files WHERE sender_person = ? AND (? IS NULL OR sender_device = ?)
         RETURNING id",
    )
    .bind(person)
    .bind(device)
    .bind(device)
    .fetch_all(&mut *db)
    .await?;
    for id in &ids {
        query("UPDATE files SET expires_at = created_at + ? WHERE id = ?")
            .bind(retention)
            .bind(id)
            .execute(&mut *db)
            .await?;
    }
    if !ids.is_empty() {
        tracing::info!(count = ids.len(), "shared files returned to normal expiry");
    }
    Ok(())
}

/// A channel's shared files, newest first, as signed records like messages.
pub async fn shared(
    State(app): State<App>,
    headers: HeaderMap,
    RoutePath(channel): RoutePath<String>,
) -> ApiResult<Json<Vec<MessageRecord>>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    messages::chat_kind(&mut tx, &actor, &channel).await?;
    let rows: Vec<messages::Row> = query_as(
        "SELECT FALSE AS backfilled, h.id, h.epoch, h.sender_person, h.sender_device, h.at,
             LENGTH(h.envelope) AS size, h.envelope
         FROM shared_files h JOIN files f ON f.id = h.id
         WHERE f.channel_id = ? AND f.expires_at > ?
         ORDER BY h.at DESC, h.id LIMIT ?",
    )
    .bind(&channel)
    .bind(actor.at)
    .bind(SHARED_LIMIT)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        rows.into_iter()
            .map(MessageRecord::try_from)
            .collect::<ApiResult<_>>()?,
    ))
}

impl Files {
    async fn sync_dir(&self) -> ApiResult<()> {
        fs::File::open(&self.dir)
            .await
            .map_err(disk_error)?
            .sync_all()
            .await
            .map_err(disk_error)
    }

    async fn remove(&self, db: &mut SqliteConnection, id: &str) -> ApiResult<()> {
        match fs::remove_file(self.dir.join(id)).await {
            Ok(()) => self.sync_dir().await?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(disk_error(error)),
        }
        query("DELETE FROM files WHERE id = ?")
            .bind(id)
            .execute(db)
            .await?;
        Ok(())
    }
}

pub async fn expire(app: App) {
    let seconds = (app.files.ceilings.retention_secs / 4).clamp(1, 30) as u64;
    let mut interval = time::interval(Duration::from_secs(seconds));
    loop {
        interval.tick().await;
        if let Err(error) = expire_batch(&app).await {
            tracing::error!(reason = error.code, "file expiry failed");
        }
    }
}

async fn expire_batch(app: &App) -> ApiResult<()> {
    let mut tx = store::write(&app.db).await?;
    let limits = file_settings::effective(&mut tx, app.files.ceilings).await?;
    let now = store::now()?;
    // Shorter retention also applies to files uploaded before the setting changed.
    // Shared files stay until removed or their channel is deleted.
    let rows: Vec<(String, i64)> = query_as(
        "SELECT id, size FROM files WHERE expires_at <= ?
             OR (created_at <= ? AND id NOT IN (SELECT id FROM shared_files))
         ORDER BY expires_at LIMIT 64",
    )
    .bind(now)
    .bind(now - limits.retention_secs)
    .fetch_all(&mut *tx)
    .await?;
    let mut removed = Vec::new();
    for (id, size) in rows {
        match app.files.remove(&mut tx, &id).await {
            Ok(()) => removed.push((id, size)),
            Err(error) => tracing::error!(reason = error.code, "file expiry failed"),
        }
    }
    tx.commit().await?;
    for (id, size) in removed {
        tracing::info!(id, size, "file expired");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person, person};
    use rusttalk_core::auth::encode;

    #[tokio::test]
    async fn saved_limits_apply_to_uploads_and_shorter_retention_waits_for_cleanup() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let channel = encode(&[3; 32]);
        query("INSERT INTO channels (id, kind, position, dm_first, dm_second) VALUES (?, 'dm', 0, ?, ?)")
            .bind(&channel)
            .bind(person(1))
            .bind(person(2))
            .execute(&app.db)
            .await?;
        let set = |limits| file_settings::set(State(app.clone()), as_person(1), Ok(Json(limits)));
        let send = |by, id, size| {
            upload(
                State(app.clone()),
                as_person(by),
                Ok(Query(Upload {
                    channel: channel.clone(),
                    id: encode(&[id; 32]),
                })),
                Body::from(vec![0; size]),
            )
        };
        let limits = FileLimits {
            max_file_bytes: 64,
            member_storage_bytes: 128,
            server_storage_bytes: 256,
            retention_secs: 300,
            max_stickers: 10,
        };
        assert_eq!(set(limits).await?.0.limits, limits);
        assert_eq!(send(1, 4, 32).await?.0, StatusCode::CREATED);
        let id = encode(&[4; 32]);
        query("UPDATE files SET created_at = created_at - 60 WHERE id = ?")
            .bind(&id)
            .execute(&app.db)
            .await?;

        let Json(updated) = set(FileLimits {
            max_file_bytes: 24,
            member_storage_bytes: 32,
            server_storage_bytes: 40,
            retention_secs: 30,
            ..limits
        })
        .await?;
        assert_eq!(updated.used_bytes, 32);
        assert_eq!(
            download(State(app.clone()), as_person(2), RoutePath(id.clone()))
                .await?
                .status(),
            StatusCode::OK
        );
        assert_eq!(send(2, 5, 25).await.unwrap_err().code, "file_too_large");
        assert_eq!(send(1, 5, 16).await.unwrap_err().code, "file_member_quota");
        assert_eq!(send(2, 5, 16).await.unwrap_err().code, "file_server_quota");

        expire_batch(&app).await?;
        assert!(!app.files.dir.join(id).exists());
        let Json(after) = file_settings::get(State(app.clone()), as_person(2)).await?;
        assert_eq!(after.used_bytes, 0);
        assert_eq!(send(2, 5, 16).await?.0, StatusCode::CREATED);
        Ok(())
    }

    /// A DM between persons 1 and 2, holding `count` shared files of person `by` from
    /// device `[by; 32]`, made directly so the test needs no signatures.
    async fn seed_shared(app: &App, by: u8, count: i64) -> Result<String> {
        let channel = encode(&[3; 32]);
        query("INSERT INTO channels (id, kind, position, dm_first, dm_second) VALUES (?, 'dm', 0, ?, ?)
               ON CONFLICT DO NOTHING")
            .bind(&channel)
            .bind(person(1))
            .bind(person(2))
            .execute(&app.db)
            .await?;
        query("INSERT INTO message_senders VALUES (?, ?, X'00') ON CONFLICT DO NOTHING")
            .bind(vec![by; 32])
            .bind(person(by))
            .execute(&app.db)
            .await?;
        query(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < ?)
             INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
                 SELECT ? || i, ?, ?, 16, 100, ? FROM n",
        )
        .bind(count)
        .bind(format!("seed{by}-"))
        .bind(&channel)
        .bind(person(by))
        .bind(i64::MAX)
        .execute(&app.db)
        .await?;
        query(
            "INSERT INTO shared_files (id, epoch, sender_device, sender_person, at, envelope)
             SELECT id, 1, ?, uploader, 100, X'00' FROM files WHERE id LIKE ?",
        )
        .bind(vec![by; 32])
        .bind(format!("seed{by}-%"))
        .execute(&app.db)
        .await?;
        Ok(channel)
    }

    fn share_body(id: [u8; 32], envelope: usize) -> Result<Json<Share>, JsonRejection> {
        Ok(Json(Share {
            id: encode(&id),
            epoch: 1,
            envelope: encode(&vec![0; envelope]),
        }))
    }

    #[tokio::test]
    async fn a_channel_keeps_at_most_500_shared_files_with_small_envelopes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let channel = seed_shared(&app, 1, SHARED_LIMIT).await?;
        let uploaded = upload(
            State(app.clone()),
            as_person(1),
            Ok(Query(Upload {
                channel: channel.clone(),
                id: encode(&[4; 32]),
            })),
            Body::from(vec![0; 32]),
        )
        .await?;
        assert_eq!(uploaded.0, StatusCode::CREATED);
        let share = |envelope| {
            share(
                State(app.clone()),
                as_person(1),
                RoutePath(channel.clone()),
                share_body([4; 32], envelope),
            )
        };
        assert_eq!(
            share(MAX_SHARED_ENVELOPE + 1).await.unwrap_err().code,
            "invalid_message"
        );
        assert_eq!(share(700).await.unwrap_err().code, "shared_limit");
        // The listing returns every shared file the cap allows.
        assert_eq!(SHARED_LIMIT, 500);
        let listed: i64 = query_scalar("SELECT COUNT(*) FROM shared_files")
            .fetch_one(&app.db)
            .await?;
        assert_eq!(listed, SHARED_LIMIT);
        query("DELETE FROM shared_files WHERE id = 'seed1-1'")
            .execute(&app.db)
            .await?;
        // Under the cap, the share goes on to the key and signature checks.
        assert_ne!(share(700).await.unwrap_err().code, "shared_limit");
        Ok(())
    }

    #[test]
    fn the_largest_share_body_fits() {
        let json = serde_json::json!({
            "id": encode(&[1; 32]),
            "epoch": i64::MAX,
            "envelope": encode(&vec![0; MAX_SHARED_ENVELOPE]),
        })
        .to_string();
        assert!(json.len() <= SHARE_BODY_LIMIT);
    }

    #[tokio::test]
    async fn leaving_or_a_revoked_device_returns_shared_files_to_normal_expiry() -> Result<()> {
        use rusttalk_core::identity::{DeviceIdentity, revoke_device};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        seed_shared(&app, 2, 2).await?;
        let kept = |id: &'static str| {
            let db = app.db.clone();
            async move {
                query_scalar::<_, i64>("SELECT expires_at FROM files WHERE id = ?")
                    .bind(id)
                    .fetch_one(&db)
                    .await
            }
        };
        crate::members::leave(State(app.clone()), as_person(2)).await?;
        let shared: i64 = query_scalar("SELECT COUNT(*) FROM shared_files")
            .fetch_one(&app.db)
            .await?;
        assert_eq!(shared, 0);
        let retention = app.files.ceilings.retention_secs;
        assert_eq!(kept("seed2-1").await?, 100 + retention);

        // A revoked device's shared files go back to normal expiry; another device's stay.
        let (words, identity) = DeviceIdentity::create("test").unwrap();
        let me = *identity.certificate.person_id();
        // Any valid device key; the revocation only names it.
        let (_, other) = DeviceIdentity::create("other").unwrap();
        let revoked = other.certificate.signing_key().to_bytes();
        query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
            .bind(me.as_slice())
            .execute(&app.db)
            .await?;
        query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, X'09', '192.0.2.1', ?, X'09')")
            .bind(store::hash(&[9; 32]).as_slice()).bind(me.as_slice()).bind(i64::MAX).execute(&app.db).await?;
        let channel = encode(&[3; 32]);
        for (id, device) in [("revoked", revoked), ("other", [6; 32])] {
            query("INSERT INTO message_senders VALUES (?, ?, X'00')")
                .bind(device.as_slice())
                .bind(me.as_slice())
                .execute(&app.db)
                .await?;
            query("INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at) VALUES (?, ?, ?, 16, 100, ?)")
                .bind(id).bind(&channel).bind(me.as_slice()).bind(i64::MAX).execute(&app.db).await?;
            query("INSERT INTO shared_files VALUES (?, 1, ?, ?, 100, X'00')")
                .bind(id)
                .bind(device.as_slice())
                .bind(me.as_slice())
                .execute(&app.db)
                .await?;
        }
        let record = revoke_device(&words, &me, revoked).unwrap().to_bytes();
        let mut headers = as_person(9);
        headers.insert(
            "authorization",
            format!("Bearer {}", encode(&[9; 32])).parse().unwrap(),
        );
        let body = serde_json::from_value(serde_json::json!({ "record": encode(&record) }))?;
        crate::auth::revoke(State(app.clone()), headers, Ok(Json(body))).await?;
        assert_eq!(kept("revoked").await?, 100 + retention);
        assert_eq!(kept("other").await?, i64::MAX);
        Ok(())
    }

    #[tokio::test]
    async fn one_member_cannot_hold_every_upload_slot() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = store::open(dir.path()).await?;
        let files = Files::open(dir.path(), &db).await?;
        let a = files
            .upload_slot([1; 32])
            .map_err(|e| anyhow::anyhow!(e.code))?;
        let _a2 = files
            .upload_slot([1; 32])
            .map_err(|e| anyhow::anyhow!(e.code))?;
        assert!(files.upload_slot([1; 32]).is_err());
        let _b = files
            .upload_slot([2; 32])
            .map_err(|e| anyhow::anyhow!(e.code))?;
        let _b2 = files
            .upload_slot([2; 32])
            .map_err(|e| anyhow::anyhow!(e.code))?;
        // Every global slot is taken, so a third member waits even with no uploads of its own.
        assert!(files.upload_slot([3; 32]).is_err());
        assert_eq!(files.uploaders.lock().unwrap().len(), 2);
        drop(a);
        let _c = files
            .upload_slot([3; 32])
            .map_err(|e| anyhow::anyhow!(e.code))?;
        assert!(files.upload_slot([1; 32]).is_err());
        Ok(())
    }
}
