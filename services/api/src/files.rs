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
    body::Body,
    extract::{Path as RoutePath, Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use rusttalk_core::auth::decode;
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
    auth::authenticate,
    messages,
    roles::{self, Permission},
    store,
};

pub struct Files {
    dir: PathBuf,
    max_file: i64,
    max_member: i64,
    max_server: i64,
    ttl: i64,
    uploads: Semaphore,
    uploaders: Mutex<HashMap<[u8; 32], usize>>,
}

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
            max_file: setting("RUSTTALK_FILE_MAX_BYTES", 32 * 1024 * 1024)?,
            max_member: setting("RUSTTALK_FILE_MEMBER_BYTES", 500 * 1024 * 1024)?,
            max_server: setting("RUSTTALK_FILE_SERVER_BYTES", 5 * 1024 * 1024 * 1024)?,
            ttl: setting("RUSTTALK_FILE_TTL_SECS", 30 * 24 * 60 * 60)?,
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

fn setting(name: &str, default: i64) -> Result<i64> {
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
    tx.commit().await?;

    let files = &app.files;
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|h| h.to_str().ok())
        .and_then(|n| n.parse::<u64>().ok())
        .is_some_and(|n| n > files.max_file as u64)
    {
        return Err(too_large());
    }
    let _slot = files.upload_slot(actor.person)?;
    let temp = tempfile::Builder::new()
        .prefix(".upload-")
        .tempfile_in(&files.dir)
        .map_err(disk_error)?;
    let (output, temp) = temp.into_parts();
    let mut output = fs::File::from_std(output);
    let size = time::timeout(Duration::from_secs(120), async {
        let mut body = body.into_data_stream();
        let mut size = 0;
        while let Some(chunk) = body
            .try_next()
            .await
            .map_err(|_| ApiError::bad_request("invalid_file_body"))?
        {
            if chunk.len() as i64 > files.max_file - size {
                return Err(too_large());
            }
            size += chunk.len() as i64;
            output.write_all(&chunk).await.map_err(disk_error)?;
        }
        Ok(size)
    })
    .await
    .map_err(|_| ApiError::new(StatusCode::REQUEST_TIMEOUT, "file_upload_timeout"))??;
    if size < 16 {
        return Err(ApiError::bad_request("invalid_file_body"));
    }
    output.sync_all().await.map_err(disk_error)?;
    drop(output);

    // Reauthorize and include quota checks, ID reservation and file installation in one write transaction.
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    messages::require_write(&mut tx, &actor, &channel).await?;
    let used: i64 = query_scalar("SELECT COALESCE(SUM(size), 0) FROM files WHERE uploader = ?")
        .bind(actor.person.as_slice())
        .fetch_one(&mut *tx)
        .await?;
    if size > files.max_member - used {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_member_quota",
        ));
    }
    let used: i64 = query_scalar("SELECT COALESCE(SUM(size), 0) FROM files")
        .fetch_one(&mut *tx)
        .await?;
    if size > files.max_server - used {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_server_quota",
        ));
    }
    let expires = actor
        .at
        .checked_add(files.ttl)
        .ok_or(ApiError::internal("file_expiry_overflow"))?;
    let inserted = query(
        "INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
    )
    .bind(&id)
    .bind(&channel)
    .bind(actor.person.as_slice())
    .bind(size)
    .bind(actor.at)
    .bind(expires)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(ApiError::new(StatusCode::CONFLICT, "file_exists"));
    }
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

fn too_large() -> ApiError {
    ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "file_too_large")
}

pub async fn download(
    State(app): State<App>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> ApiResult<Response> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await.map_err(|error| {
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

pub async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let row = lookup(&mut tx, &id, actor.at).await?;
    if row.uploader != actor.person {
        messages::chat_kind(&mut tx, &actor, &row.channel_id).await?;
        roles::authority(&mut tx, &actor.person)
            .await?
            .require(Permission::DeleteMessages)?;
    }
    tokio::spawn(
        async move {
            app.files.remove(&mut tx, &id).await?;
            tx.commit().await?;
            tracing::info!(id, size = row.size, "file deleted");
            Ok(StatusCode::NO_CONTENT)
        }
        .in_current_span(),
    )
    .await
    .map_err(|_| ApiError::internal("file_task_failed"))?
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
    let mut interval = time::interval(Duration::from_secs((app.files.ttl / 4).clamp(1, 30) as u64));
    loop {
        interval.tick().await;
        let result = async {
            let mut tx = store::write(&app.db).await?;
            let rows: Vec<(String, i64)> = query_as(
                "SELECT id, size FROM files WHERE expires_at <= ? ORDER BY expires_at LIMIT 64",
            )
            .bind(store::now()?)
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
            Ok::<_, ApiError>(())
        }
        .await;
        if let Err(error) = result {
            tracing::error!(reason = error.code, "file expiry failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
