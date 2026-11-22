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
    max_packs: i64,
}

/// The pack every server starts with (the migration creates it with an empty name).
#[cfg(test)]
const DEFAULT_PACK: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

impl Emoji {
    pub async fn open(data: &FsPath, db: &SqlitePool) -> Result<Self> {
        let emoji = Self {
            dir: data.join("emoji"),
            max_emoji: setting("RUSTTALK_EMOJI_MAX", 100)?,
            max_bytes: setting("RUSTTALK_EMOJI_MAX_BYTES", IMAGE_CIPHERTEXT as i64)?,
            max_packs: setting("RUSTTALK_EMOJI_PACKS_MAX", 20)?,
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
    pack_id: String,
}

#[derive(Serialize)]
pub struct Item {
    id: String,
    kind: String,
    owner: String,
    size: i64,
    image_hash: String,
    name_ct: String,
    pack_id: String,
}

#[derive(FromRow)]
struct PackRow {
    id: String,
    name_ct: Vec<u8>,
}

#[derive(Serialize)]
pub struct Pack {
    id: String,
    name_ct: String,
}

#[derive(Serialize)]
pub struct List {
    max_emoji: i64,
    max_stickers: i64,
    max_bytes: i64,
    max_packs: i64,
    /// In display order.
    packs: Vec<Pack>,
    items: Vec<Item>,
}

pub async fn list(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<List>> {
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let max_stickers = file_settings::effective(&mut tx, app.files.ceilings)
        .await?
        .max_stickers;
    let rows: Vec<Row> = query_as(
        "SELECT id, kind, owner, size, image_hash, name_ct, pack_id FROM emoji
         ORDER BY created_at, id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let packs: Vec<PackRow> = query_as("SELECT id, name_ct FROM emoji_packs ORDER BY position, id")
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let limits = &app.emoji;
    Ok(Json(List {
        max_emoji: limits.max_emoji,
        max_stickers,
        max_bytes: limits.max_bytes,
        max_packs: limits.max_packs,
        packs: packs
            .into_iter()
            .map(|pack| Pack {
                id: pack.id,
                name_ct: encode(&pack.name_ct),
            })
            .collect(),
        items: rows
            .into_iter()
            .map(|row| Item {
                id: row.id,
                kind: row.kind,
                owner: encode(&row.owner),
                size: row.size,
                image_hash: row.image_hash,
                name_ct: encode(&row.name_ct),
                pack_id: row.pack_id,
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
    /// The pack to put it in; the first one when left out.
    #[serde(default)]
    pack_id: Option<String>,
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
        pack_id,
    } = payload(body)?;
    decode::<32>(&id)?;
    if let Some(pack) = &pack_id {
        decode::<32>(pack)?;
    }
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
    let pack = match pack_id {
        Some(pack) => {
            pack_exists(&mut tx, &pack).await?;
            pack
        }
        None => {
            query_scalar("SELECT id FROM emoji_packs ORDER BY position, id LIMIT 1")
                .fetch_one(&mut *tx)
                .await?
        }
    };
    let count: i64 = query_scalar("SELECT COUNT(*) FROM emoji WHERE kind = ?")
        .bind(kind_name)
        .fetch_one(&mut *tx)
        .await?;
    if count >= max {
        return Err(ApiError::new(StatusCode::CONFLICT, full));
    }
    let added = query(
        "INSERT INTO emoji (id, kind, owner, size, image_hash, name_ct, created_at, pack_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
    )
    .bind(&id)
    .bind(kind_name)
    .bind(actor.person.as_slice())
    .bind(image.len() as i64)
    .bind(picture_hash(&image))
    .bind(&name)
    .bind(actor.at)
    .bind(&pack)
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

async fn pack_exists(tx: &mut sqlx::SqliteConnection, id: &str) -> ApiResult<()> {
    let exists: bool = query_scalar("SELECT EXISTS (SELECT 1 FROM emoji_packs WHERE id = ?)")
        .bind(id)
        .fetch_one(tx)
        .await?;
    exists
        .then_some(())
        .ok_or_else(|| ApiError::not_found("pack_not_found"))
}

/// What every pack change is called in the audit log.
const PACK_CHANGED: &str = "emoji_pack_changed";

#[derive(Deserialize)]
pub struct NewPack {
    id: String,
    name_ct: String,
}

pub async fn add_pack(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewPack>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let NewPack { id, name_ct } = payload(body)?;
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageEmoji)?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let name = checked(Metadata::EmojiPackName, epoch, name_ct)?;
    let (count, last): (i64, Option<i64>) =
        query_as("SELECT COUNT(*), MAX(position) FROM emoji_packs")
            .fetch_one(&mut *tx)
            .await?;
    if count >= app.emoji.max_packs {
        return Err(ApiError::new(StatusCode::CONFLICT, "pack_limit"));
    }
    let added = query(
        "INSERT INTO emoji_packs (id, name_ct, position, created_at)
         VALUES (?, ?, ?, ?) ON CONFLICT (id) DO NOTHING",
    )
    .bind(&id)
    .bind(&name)
    .bind(last.map_or(0, |last| last + 1))
    .bind(actor.at)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if added == 0 {
        return Err(ApiError::new(StatusCode::CONFLICT, "pack_exists"));
    }
    store::audit(
        &mut tx,
        &actor.person,
        PACK_CHANGED,
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::CREATED)
}

pub async fn rename_pack(
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
    let name = checked(Metadata::EmojiPackName, epoch, name_ct)?;
    let changed = query("UPDATE emoji_packs SET name_ct = ? WHERE id = ?")
        .bind(name)
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if changed == 0 {
        return Err(ApiError::not_found("pack_not_found"));
    }
    store::audit(
        &mut tx,
        &actor.person,
        PACK_CHANGED,
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct PackOrder {
    ids: Vec<String>,
}

/// Put the packs in the listed order; the list must hold every pack exactly once.
pub async fn order_packs(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<PackOrder>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let PackOrder { ids } = payload(body)?;
    let mut given = std::collections::BTreeSet::new();
    for id in &ids {
        decode::<32>(id)?;
        if !given.insert(id.as_str()) {
            return Err(ApiError::bad_request("invalid_order"));
        }
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageEmoji)?;
    let known: Vec<String> = query_scalar("SELECT id FROM emoji_packs ORDER BY id")
        .fetch_all(&mut *tx)
        .await?;
    if !given.iter().copied().eq(known.iter().map(String::as_str)) {
        return Err(keys::conflict("order_changed"));
    }
    for (position, id) in (0_i64..).zip(&ids) {
        query("UPDATE emoji_packs SET position = ? WHERE id = ?")
            .bind(position)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    store::audit(&mut tx, &actor.person, PACK_CHANGED, None, None, actor.at).await?;
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a pack and the emoji and stickers in it. The last pack stays: items need one.
pub async fn delete_pack(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageEmoji)?;
    pack_exists(&mut tx, &id).await?;
    let packs: i64 = query_scalar("SELECT COUNT(*) FROM emoji_packs")
        .fetch_one(&mut *tx)
        .await?;
    if packs <= 1 {
        return Err(ApiError::new(StatusCode::CONFLICT, "last_pack"));
    }
    let items: Vec<String> = query_scalar("SELECT id FROM emoji WHERE pack_id = ?")
        .bind(&id)
        .fetch_all(&mut *tx)
        .await?;
    query("DELETE FROM emoji WHERE pack_id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    // Each emoji or sticker that goes with the pack is a delete in the log, as on its own.
    for item in &items {
        store::audit(
            &mut tx,
            &actor.person,
            "emoji_deleted",
            None,
            Some(item),
            actor.at,
        )
        .await?;
    }
    query("DELETE FROM emoji_packs WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    store::audit(
        &mut tx,
        &actor.person,
        PACK_CHANGED,
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    // The rows are gone; a file left by a failure here is removed at the next start.
    for item in items {
        match fs::remove_file(app.emoji.path(&item)).await {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                disk_error(error);
            }
        }
    }
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Move {
    pack_id: String,
}

pub async fn move_to_pack(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<Move>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Move { pack_id } = payload(body)?;
    decode::<32>(&id)?;
    decode::<32>(&pack_id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageEmoji)?;
    pack_exists(&mut tx, &pack_id).await?;
    let changed = query("UPDATE emoji SET pack_id = ? WHERE id = ?")
        .bind(&pack_id)
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
        PACK_CHANGED,
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
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
                pack_id: None,
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

    /// A server with an initialised space key: person 1 owns it, the rest are plain members.
    async fn space(
        dir: &std::path::Path,
        people: u8,
    ) -> Result<(App, [u8; 32], rusttalk_core::keys::Key)> {
        let app = app(dir, people).await?;
        let server = keys::space(&app).id;
        let mut conn = app.db.acquire().await?;
        keys::init_space(&mut conn, &server).await?;
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(encode(&server))
            .execute(&mut *conn)
            .await?;
        drop(conn);
        Ok((app, server, rusttalk_core::keys::random_key()?))
    }

    fn seal(
        server: &[u8; 32],
        key: &rusttalk_core::keys::Key,
        kind: Metadata,
        id: &[u8; 32],
    ) -> String {
        encode(&kind.seal(server, id, 1, key, b"sample").unwrap())
    }

    fn sample(
        server: &[u8; 32],
        key: &rusttalk_core::keys::Key,
        byte: u8,
        pack: Option<&[u8; 32]>,
    ) -> New {
        New {
            id: encode(&[byte; 32]),
            kind: Kind::Emoji,
            name_ct: seal(server, key, Metadata::EmojiName, &[byte; 32]),
            image_ct: seal(server, key, Metadata::EmojiImage, &[byte; 32]),
            pack_id: pack.map(|pack| encode(pack)),
        }
    }

    #[tokio::test]
    async fn items_start_in_the_default_pack_and_move_between_packs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (app, server, key) = space(dir.path(), 2).await?;
        let new_pack = |byte: u8| NewPack {
            id: encode(&[byte; 32]),
            name_ct: seal(&server, &key, Metadata::EmojiPackName, &[byte; 32]),
        };
        add(
            State(app.clone()),
            as_person(1),
            Ok(Json(sample(&server, &key, 3, None))),
        )
        .await?;
        let Json(now) = list(State(app.clone()), as_person(2)).await?;
        assert_eq!(now.packs.len(), 1);
        assert_eq!(now.packs[0].id, DEFAULT_PACK);
        assert_eq!(now.items[0].pack_id, DEFAULT_PACK);
        // Creating, adding to a new pack, moving, reordering.
        add_pack(State(app.clone()), as_person(1), Ok(Json(new_pack(9)))).await?;
        add(
            State(app.clone()),
            as_person(1),
            Ok(Json(sample(&server, &key, 4, Some(&[9; 32])))),
        )
        .await?;
        move_to_pack(
            State(app.clone()),
            as_person(1),
            Path(encode(&[3; 32])),
            Ok(Json(Move {
                pack_id: encode(&[9; 32]),
            })),
        )
        .await?;
        order_packs(
            State(app.clone()),
            as_person(1),
            Ok(Json(PackOrder {
                ids: vec![encode(&[9; 32]), DEFAULT_PACK.into()],
            })),
        )
        .await?;
        let Json(now) = list(State(app.clone()), as_person(2)).await?;
        let order: Vec<_> = now.packs.iter().map(|pack| pack.id.as_str()).collect();
        assert_eq!(order, [encode(&[9; 32]).as_str(), DEFAULT_PACK]);
        assert!(
            now.items
                .iter()
                .all(|item| item.pack_id == encode(&[9; 32]))
        );
        // Unknown packs are refused when adding or moving.
        let missing_pack = add(
            State(app.clone()),
            as_person(1),
            Ok(Json(sample(&server, &key, 5, Some(&[77; 32])))),
        )
        .await;
        assert_eq!(missing_pack.unwrap_err().code, "pack_not_found");
        Ok(())
    }

    #[tokio::test]
    async fn only_people_who_manage_emoji_change_packs() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (app, server, key) = space(dir.path(), 2).await?;
        let body = || NewPack {
            id: encode(&[9; 32]),
            name_ct: seal(&server, &key, Metadata::EmojiPackName, &[9; 32]),
        };
        let denied = add_pack(State(app.clone()), as_person(2), Ok(Json(body()))).await;
        assert_eq!(denied.unwrap_err().code, "permission_denied");
        add_pack(State(app.clone()), as_person(1), Ok(Json(body()))).await?;
        let rename = || Rename {
            name_ct: seal(&server, &key, Metadata::EmojiPackName, &[9; 32]),
        };
        let denied = rename_pack(
            State(app.clone()),
            as_person(2),
            Path(encode(&[9; 32])),
            Ok(Json(rename())),
        )
        .await;
        assert_eq!(denied.unwrap_err().code, "permission_denied");
        rename_pack(
            State(app.clone()),
            as_person(1),
            Path(encode(&[9; 32])),
            Ok(Json(rename())),
        )
        .await?;
        let denied = delete_pack(State(app.clone()), as_person(2), Path(encode(&[9; 32]))).await;
        assert_eq!(denied.unwrap_err().code, "permission_denied");
        // A name sealed for another kind of object is refused by its size limits only, but
        // a name under the wrong epoch is refused.
        let stale = Rename {
            name_ct: encode(&Metadata::EmojiPackName.seal(&server, &[9; 32], 2, &key, b"x")?),
        };
        let stale = rename_pack(
            State(app.clone()),
            as_person(1),
            Path(encode(&[9; 32])),
            Ok(Json(stale)),
        )
        .await;
        assert_eq!(stale.unwrap_err().code, "metadata_epoch_changed");
        Ok(())
    }

    #[tokio::test]
    async fn pack_limits_and_deleting_a_pack_removes_its_pictures() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (mut app, server, key) = space(dir.path(), 1).await?;
        let mut emoji = Emoji::open(dir.path(), &app.db).await?;
        emoji.max_packs = 2;
        app.emoji = emoji.into();
        let new_pack = |byte: u8| NewPack {
            id: encode(&[byte; 32]),
            name_ct: seal(&server, &key, Metadata::EmojiPackName, &[byte; 32]),
        };
        add_pack(State(app.clone()), as_person(1), Ok(Json(new_pack(9)))).await?;
        let full = add_pack(State(app.clone()), as_person(1), Ok(Json(new_pack(10)))).await;
        assert_eq!(full.unwrap_err().code, "pack_limit");
        let again = add_pack(State(app.clone()), as_person(1), Ok(Json(new_pack(9)))).await;
        assert_eq!(again.unwrap_err().code, "pack_limit");
        add(
            State(app.clone()),
            as_person(1),
            Ok(Json(sample(&server, &key, 4, Some(&[9; 32])))),
        )
        .await?;
        let image = app.emoji.path(&encode(&[4; 32]));
        assert!(image.exists());
        delete_pack(State(app.clone()), as_person(1), Path(encode(&[9; 32]))).await?;
        assert!(!image.exists());
        let deleted: i64 =
            query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'emoji_deleted'")
                .fetch_one(&app.db)
                .await?;
        assert_eq!(deleted, 1);
        let Json(now) = list(State(app.clone()), as_person(1)).await?;
        assert!(now.items.is_empty());
        assert_eq!(now.packs.len(), 1);
        // The last pack stays.
        let last = delete_pack(State(app.clone()), as_person(1), Path(DEFAULT_PACK.into())).await;
        assert_eq!(last.unwrap_err().code, "last_pack");
        Ok(())
    }

    #[tokio::test]
    async fn pack_order_must_list_every_pack_once() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (app, _, _) = space(dir.path(), 1).await?;
        let order = |ids: Vec<String>| {
            order_packs(
                State(app.clone()),
                as_person(1),
                Ok(Json(PackOrder { ids })),
            )
        };
        let twice = order(vec![DEFAULT_PACK.into(), DEFAULT_PACK.into()]).await;
        assert_eq!(twice.unwrap_err().code, "invalid_order");
        let wrong = order(vec![encode(&[5; 32])]).await;
        assert_eq!(wrong.unwrap_err().code, "order_changed");
        order(vec![DEFAULT_PACK.into()]).await?;
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
