//! Server emoji and stickers. Names and images are space-key ciphertext; the server
//! only knows IDs, kinds, sizes and who added them. Images live on disk, not in SQLite.

use std::{
    io::{self, Write},
    path::{Path as FsPath, PathBuf},
};

use anyhow::{Result, ensure};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, decode_bytes, encode},
    keys::{Event, IMAGE_CIPHERTEXT, Metadata},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, SqlitePool, query, query_as, query_scalar};
use tokio::fs;

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    file_settings,
    files::setting,
    keys,
    metadata::{checked, picture_hash},
    payload,
    roles::{self, Permission},
    store,
};

pub struct Emoji {
    dir: PathBuf,
    max_emoji: i64,
    max_bytes: i64,
}

impl Emoji {
    pub async fn open(data: &FsPath, db: &SqlitePool) -> Result<Self> {
        let emoji = Self {
            dir: data.join("emoji"),
            max_emoji: setting("RUSTTALK_EMOJI_MAX", 100)?,
            max_bytes: setting("RUSTTALK_EMOJI_MAX_BYTES", IMAGE_CIPHERTEXT as i64)?,
        };
        ensure!(
            emoji.max_bytes <= IMAGE_CIPHERTEXT as i64,
            "RUSTTALK_EMOJI_MAX_BYTES must be at most {IMAGE_CIPHERTEXT}"
        );
        fs::create_dir_all(&emoji.dir).await?;
        // A request dropped between writing the image and committing its row leaves a file
        // without a row; a delete removes the row first. Either way the file goes here.
        let mut entries = fs::read_dir(&emoji.dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let known = match name.to_str() {
                Some(id) if decode::<32>(id).is_ok() => {
                    query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM emoji WHERE id = ?)")
                        .bind(id)
                        .fetch_one(db)
                        .await?
                }
                _ => false,
            };
            if !known && entry.file_type().await?.is_file() {
                fs::remove_file(entry.path()).await?;
            }
        }
        Ok(emoji)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }
}

fn disk_error(error: io::Error) -> ApiError {
    tracing::warn!(kind = ?error.kind(), "emoji storage I/O failed");
    ApiError::internal("emoji_storage_error")
}

fn missing() -> ApiError {
    ApiError::not_found("emoji_not_found")
}

#[derive(FromRow)]
struct Row {
    id: String,
    kind: String,
    owner: Vec<u8>,
    size: i64,
    image_hash: String,
    name_ct: Vec<u8>,
}

#[derive(Serialize)]
pub struct Item {
    id: String,
    kind: String,
    owner: String,
    size: i64,
    image_hash: String,
    name_ct: String,
}

#[derive(Serialize)]
pub struct List {
    max_emoji: i64,
    max_stickers: i64,
    max_bytes: i64,
    items: Vec<Item>,
}

pub async fn list(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<List>> {
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let max_stickers = file_settings::effective(&mut tx, app.files.ceilings)
        .await?
        .max_stickers;
    let rows: Vec<Row> = query_as(
        "SELECT id, kind, owner, size, image_hash, name_ct FROM emoji ORDER BY created_at, id",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let limits = &app.emoji;
    Ok(Json(List {
        max_emoji: limits.max_emoji,
        max_stickers,
        max_bytes: limits.max_bytes,
        items: rows
            .into_iter()
            .map(|row| Item {
                id: row.id,
                kind: row.kind,
                owner: encode(&row.owner),
                size: row.size,
                image_hash: row.image_hash,
                name_ct: encode(&row.name_ct),
            })
            .collect(),
    }))
}

pub async fn image(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    decode::<32>(&id)?;
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let exists: bool = query_scalar("SELECT EXISTS (SELECT 1 FROM emoji WHERE id = ?)")
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    if !exists {
        return Err(missing());
    }
    let image = fs::read(app.emoji.path(&id)).await.map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            missing()
        } else {
            disk_error(error)
        }
    })?;
    Ok(Json(json!({ "image_ct": encode(&image) })))
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Emoji,
    Sticker,
}

#[derive(Deserialize)]
pub struct New {
    id: String,
    kind: Kind,
    name_ct: String,
    image_ct: String,
}

pub async fn add(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<New>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let New {
        id,
        kind,
        name_ct,
        image_ct,
    } = payload(body)?;
    decode::<32>(&id)?;
    let limits = &app.emoji;
    if decode_bytes(&image_ct)?.len() as i64 > limits.max_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "emoji_too_large",
        ));
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageEmoji)?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let name = checked(Metadata::EmojiName, epoch, name_ct)?;
    let image = checked(Metadata::EmojiImage, epoch, image_ct)?;
    let (kind_name, max, full) = match kind {
        Kind::Emoji => ("emoji", limits.max_emoji, "emoji_limit"),
        Kind::Sticker => (
            "sticker",
            file_settings::effective(&mut tx, app.files.ceilings)
                .await?
                .max_stickers,
            "sticker_limit",
        ),
    };
    let count: i64 = query_scalar("SELECT COUNT(*) FROM emoji WHERE kind = ?")
        .bind(kind_name)
        .fetch_one(&mut *tx)
        .await?;
    if count >= max {
        return Err(ApiError::new(StatusCode::CONFLICT, full));
    }
    let added = query(
        "INSERT INTO emoji (id, kind, owner, size, image_hash, name_ct, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
    )
    .bind(&id)
    .bind(kind_name)
    .bind(actor.person.as_slice())
    .bind(image.len() as i64)
    .bind(picture_hash(&image))
    .bind(&name)
    .bind(actor.at)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if added == 0 {
        return Err(ApiError::new(StatusCode::CONFLICT, "emoji_exists"));
    }
    store::audit(
        &mut tx,
        &actor.person,
        "emoji_added",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    // Write the image before the row commits; startup removes it if the commit never happens.
    let dir = limits.dir.clone();
    let path = limits.path(&id);
    tokio::task::spawn_blocking(move || {
        let mut file = tempfile::Builder::new()
            .prefix(".upload-")
            .tempfile_in(&dir)?;
        file.write_all(&image)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&path).map_err(|error| error.error)?;
        std::fs::File::open(&dir)?.sync_all()
    })
    .await
    .map_err(|_| ApiError::internal("emoji_task_failed"))?
    .map_err(disk_error)?;
    tx.commit().await?;
    tracing::info!(id, kind = kind_name, "emoji added");
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::CREATED)
}

#[derive(Deserialize)]
pub struct Rename {
    name_ct: String,
}

pub async fn rename(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<Rename>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Rename { name_ct } = payload(body)?;
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageEmoji)?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let name = checked(Metadata::EmojiName, epoch, name_ct)?;
    let changed = query("UPDATE emoji SET name_ct = ? WHERE id = ?")
        .bind(name)
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if changed == 0 {
        return Err(missing());
    }
    store::audit(
        &mut tx,
        &actor.person,
        "emoji_renamed",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageEmoji)?;
    let removed = query("DELETE FROM emoji WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if removed == 0 {
        return Err(missing());
    }
    store::audit(
        &mut tx,
        &actor.person,
        "emoji_deleted",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    // The row is gone; a file left by a failure here is removed at the next start.
    match fs::remove_file(app.emoji.path(&id)).await {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            disk_error(error);
        }
    }
    tracing::info!(id, "emoji deleted");
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person};

    #[tokio::test]
    async fn saved_sticker_limit_keeps_existing_stickers_and_blocks_new_ones() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let server = keys::space(&app).id;
        let mut conn = app.db.acquire().await?;
        keys::init_space(&mut conn, &server).await?;
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(encode(&server))
            .execute(&mut *conn)
            .await?;
        drop(conn);
        let key = rusttalk_core::keys::random_key()?;
        let add_sticker = |byte| -> Result<New> {
            let id = [byte; 32];
            let seal = |kind: Metadata| {
                kind.seal(&server, &id, 1, &key, b"sample")
                    .map(|ct| encode(&ct))
            };
            Ok(New {
                id: encode(&id),
                kind: Kind::Sticker,
                name_ct: seal(Metadata::EmojiName)?,
                image_ct: seal(Metadata::EmojiImage)?,
            })
        };
        add(State(app.clone()), as_person(1), Ok(Json(add_sticker(3)?))).await?;
        add(State(app.clone()), as_person(1), Ok(Json(add_sticker(4)?))).await?;
        let Json(saved) = file_settings::set(
            State(app.clone()),
            as_person(1),
            Ok(Json(rusttalk_core::auth::FileLimits {
                max_stickers: 1,
                ..app.files.ceilings
            })),
        )
        .await?;
        assert_eq!(saved.limits.max_stickers, 1);
        let Json(list) = list(State(app.clone()), as_person(2)).await?;
        assert_eq!(list.max_stickers, 1);
        assert_eq!(list.items.len(), 2);
        let error = add(State(app), as_person(1), Ok(Json(add_sticker(5)?)))
            .await
            .unwrap_err();
        assert_eq!(error.code, "sticker_limit");
        Ok(())
    }

    #[tokio::test]
    async fn startup_removes_images_without_rows() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db = store::open(dir.path()).await?;
        let kept = encode(&[1; 32]);
        let orphan = encode(&[2; 32]);
        std::fs::create_dir_all(dir.path().join("emoji"))?;
        for name in [&kept, &orphan, &".upload-x".to_owned()] {
            std::fs::write(dir.path().join("emoji").join(name), b"ciphertext")?;
        }
        query(
            "INSERT INTO emoji (id, kind, owner, size, image_hash, name_ct, created_at)
             VALUES (?, 'emoji', ?, 10, 'h', ?, 1)",
        )
        .bind(&kept)
        .bind([3; 32].as_slice())
        .bind([4; 8].as_slice())
        .execute(&db)
        .await?;
        let emoji = Emoji::open(dir.path(), &db).await?;
        let mut left: Vec<_> = std::fs::read_dir(&emoji.dir)?
            .map(|entry| entry.map(|e| e.file_name().into_string().unwrap()))
            .collect::<io::Result<_>>()?;
        left.sort();
        assert_eq!(left, [kept]);
        assert_eq!(emoji.max_emoji, 100);
        assert_eq!(emoji.max_bytes, IMAGE_CIPHERTEXT as i64);
        Ok(())
    }
}
