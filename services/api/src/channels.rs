use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, encode},
    keys::{Event, Kind, Metadata, Scope},
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate},
    keys, metadata, payload,
    roles::{self, Permission},
    store::audit,
};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChannelKind {
    Text,
    Voice,
}

#[derive(Deserialize)]
pub struct ChannelFields {
    category_id: String,
    position: i64,
    name_ct: String,
    topic_ct: Option<String>,
}

#[derive(Deserialize)]
pub struct NewChannel {
    id: String,
    kind: ChannelKind,
    commitment: String,
    envelope: String,
    #[serde(flatten)]
    fields: ChannelFields,
}

#[derive(FromRow)]
struct ChannelRow {
    id: String,
    kind: String,
    category_id: Option<String>,
    position: i64,
    name_ct: Option<Vec<u8>>,
    topic_ct: Option<Vec<u8>>,
    peer: Option<Vec<u8>>,
}

#[derive(Serialize)]
pub struct Channel {
    id: String,
    kind: String,
    category_id: Option<String>,
    position: i64,
    name_ct: Option<String>,
    topic_ct: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    peer: Option<String>,
    flags: Vec<RoleFlags>,
}

#[derive(Deserialize, FromRow, Serialize)]
pub struct RoleFlags {
    role_id: String,
    flags: u8,
}

#[derive(FromRow)]
struct CategoryRow {
    id: String,
    position: i64,
    name_ct: Vec<u8>,
}

#[derive(Serialize)]
pub struct Category {
    id: String,
    position: i64,
    name_ct: String,
}

#[derive(Deserialize)]
pub struct CategoryFields {
    position: i64,
    name_ct: String,
}

#[derive(Deserialize)]
pub struct NewCategory {
    id: String,
    #[serde(flatten)]
    fields: CategoryFields,
}

#[derive(Serialize)]
pub struct Catalog {
    channels: Vec<Channel>,
    categories: Vec<Category>,
}

async fn manage(db: &mut SqliteConnection, actor: &Actor) -> ApiResult<()> {
    roles::authority(db, &actor.person)
        .await?
        .require(Permission::ManageChannels)
}

async fn channels(db: &mut SqliteConnection, person: Option<&[u8]>) -> ApiResult<Vec<Channel>> {
    let rows: Vec<ChannelRow> = query_as(
        "SELECT id, kind, category_id, position, name_ct, topic_ct,
                CASE WHEN dm_first = ? THEN dm_second ELSE dm_first END AS peer
         FROM channels c
         WHERE (? IS NULL AND kind <> 'dm') OR EXISTS
             (SELECT 1 FROM channel_readers r WHERE r.channel_id = c.id AND r.person_id = ?)
         ORDER BY position, id",
    )
    .bind(person)
    .bind(person)
    .bind(person)
    .fetch_all(&mut *db)
    .await?;
    let mut channels = Vec::with_capacity(rows.len());
    for row in rows {
        let flags = query_as(
            "SELECT role_id, flags FROM channel_permissions WHERE channel_id = ? ORDER BY role_id",
        )
        .bind(&row.id)
        .fetch_all(&mut *db)
        .await?;
        channels.push(Channel {
            id: row.id,
            kind: row.kind,
            category_id: row.category_id,
            position: row.position,
            name_ct: metadata::encoded(row.name_ct),
            peer: metadata::encoded(row.peer),
            topic_ct: metadata::encoded(row.topic_ct),
            flags,
        });
    }
    Ok(channels)
}

async fn categories(db: &mut SqliteConnection, person: Option<&[u8]>) -> ApiResult<Vec<Category>> {
    let rows: Vec<CategoryRow> = query_as(
        "SELECT id, position, name_ct FROM categories a
         WHERE ? IS NULL OR EXISTS (SELECT 1 FROM channels c JOIN channel_readers r
                                    ON c.id = r.channel_id WHERE c.category_id = a.id AND r.person_id = ?)
         ORDER BY position, id",
    ).bind(person).bind(person).fetch_all(db).await?;
    Ok(rows
        .into_iter()
        .map(|row| Category {
            id: row.id,
            position: row.position,
            name_ct: encode(&row.name_ct),
        })
        .collect())
}

pub async fn catalog(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Catalog>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let person = Some(actor.person.as_slice());
    let catalog = Catalog {
        channels: channels(&mut tx, person).await?,
        categories: categories(&mut tx, person).await?,
    };
    tx.commit().await?;
    Ok(Json(catalog))
}

pub async fn list_channels(
    State(app): State<App>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Channel>>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    let list = channels(&mut tx, None).await?;
    tx.commit().await?;
    Ok(Json(list))
}

pub async fn list_categories(
    State(app): State<App>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Category>>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    let list = categories(&mut tx, None).await?;
    tx.commit().await?;
    Ok(Json(list))
}

async fn category_exists(db: &mut SqliteConnection, id: &str) -> ApiResult<()> {
    decode::<32>(id)?;
    if !query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM categories WHERE id = ?)")
        .bind(id)
        .fetch_one(db)
        .await?
    {
        return Err(ApiError::not_found("category_not_found"));
    }
    Ok(())
}

pub async fn create_channel(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewChannel>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let NewChannel {
        id,
        kind,
        commitment,
        envelope,
        fields,
    } = payload(body)?;
    let scope = Scope {
        kind: Kind::Channel,
        id: decode(&id)?,
    };
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    category_exists(&mut tx, &fields.category_id).await?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let new = query("INSERT INTO channel_ids (id) VALUES (?) ON CONFLICT (id) DO NOTHING")
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if new == 0 || scope.id == keys::space(&app).id {
        return Err(keys::conflict("channel_exists"));
    }
    query("INSERT INTO channels (id, kind, category_id, position, name_ct, topic_ct) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&id).bind(match kind { ChannelKind::Text => "text", ChannelKind::Voice => "voice" })
        .bind(&fields.category_id).bind(fields.position)
        .bind(metadata::checked(Metadata::ChannelName, epoch, fields.name_ct)?)
        .bind(metadata::optional(Metadata::ChannelTopic, epoch, fields.topic_ct)?)
        .execute(&mut *tx).await?;
    query(
        "INSERT INTO channel_permissions (channel_id, role_id, flags)
           SELECT ?, id, 15 FROM roles WHERE id IN ('owner', 'admin', 'member')",
    )
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    // A custom channel manager can create a channel even without a built-in reader role.
    let can_read: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM channel_readers WHERE channel_id = ? AND person_id = ?)",
    )
    .bind(&id)
    .bind(actor.person.as_slice())
    .fetch_one(&mut *tx)
    .await?;
    if !can_read {
        query("INSERT INTO channel_permissions (channel_id, role_id, flags)
               SELECT ?, r.id, 15 FROM member_roles m JOIN roles r ON m.role_id = r.id
               WHERE m.person_id = ? AND (r.permissions & 64) <> 0 ORDER BY r.rank DESC, r.id LIMIT 1")
            .bind(&id).bind(actor.person.as_slice()).execute(&mut *tx).await?;
    }
    keys::start_channel(
        &mut tx,
        &app,
        &actor,
        scope,
        decode(&commitment)?,
        &envelope,
    )
    .await?;
    audit(
        &mut tx,
        &actor.person,
        "channel_created",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Some(Event::ChannelChanged));
    Ok(StatusCode::CREATED)
}

pub async fn update_channel(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<ChannelFields>, JsonRejection>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let fields = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    category_exists(&mut tx, &fields.category_id).await?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let changed = query(
        "UPDATE channels SET category_id = ?, position = ?, name_ct = ?, topic_ct = ? WHERE id = ? AND kind <> 'dm'",
    )
    .bind(&fields.category_id)
    .bind(fields.position)
    .bind(metadata::checked(
        Metadata::ChannelName,
        epoch,
        fields.name_ct,
    )?)
    .bind(metadata::optional(
        Metadata::ChannelTopic,
        epoch,
        fields.topic_ct,
    )?)
    .bind(&id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(ApiError::not_found("channel_not_found"));
    }
    audit(
        &mut tx,
        &actor.person,
        "channel_updated",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Some(Event::ChannelChanged));
    Ok(StatusCode::NO_CONTENT)
}

pub async fn permissions(
    State(app): State<App>,
    headers: HeaderMap,
    Path((id, role)): Path<(String, String)>,
    body: Result<Json<PermissionFields>, JsonRejection>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let flags = payload(body)?.flags;
    if flags > 15 || (flags != 0 && flags & 1 == 0) {
        return Err(ApiError::bad_request("invalid_channel_flags"));
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let authority = roles::authority(&mut tx, &actor.person).await?;
    authority.require(Permission::ManageChannels)?;
    let exists: bool =
        query_scalar("SELECT EXISTS (SELECT 1 FROM channels WHERE id = ? AND kind <> 'dm')")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Err(ApiError::not_found("channel_not_found"));
    }
    authority.can_set_channel_flags(roles::by_id(&mut tx, &role).await?)?;
    let before = readers(&mut tx, &id).await?;
    if flags == 0 {
        query("DELETE FROM channel_permissions WHERE channel_id = ? AND role_id = ?")
            .bind(&id)
            .bind(&role)
            .execute(&mut *tx)
            .await?;
    } else {
        query(
            "INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES (?, ?, ?)
            ON CONFLICT (channel_id, role_id) DO UPDATE SET flags = excluded.flags",
        )
        .bind(&id)
        .bind(&role)
        .bind(flags)
        .execute(&mut *tx)
        .await?;
    }
    let after = readers(&mut tx, &id).await?;
    if !before.is_subset(&after) {
        keys::mark(&mut tx, &id).await?;
    }
    audit(
        &mut tx,
        &actor.person,
        "channel_permissions_changed",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Some(Event::ChannelChanged));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct PermissionFields {
    flags: u8,
}

async fn readers(
    db: &mut SqliteConnection,
    id: &str,
) -> ApiResult<std::collections::HashSet<Vec<u8>>> {
    let members: Vec<Vec<u8>> =
        query_scalar("SELECT person_id FROM channel_readers WHERE channel_id = ?")
            .bind(id)
            .fetch_all(db)
            .await?;
    Ok(members.into_iter().collect())
}

pub async fn delete_channel(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    let deleted = query("DELETE FROM channels WHERE id = ? AND kind <> 'dm'")
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(ApiError::not_found("channel_not_found"));
    }
    audit(
        &mut tx,
        &actor.person,
        "channel_deleted",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Some(Event::ChannelChanged));
    Ok(StatusCode::NO_CONTENT)
}

pub async fn create_category(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewCategory>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let NewCategory { id, fields } = payload(body)?;
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let inserted = query("INSERT INTO categories (id, position, name_ct) VALUES (?, ?, ?) ON CONFLICT (id) DO NOTHING")
        .bind(&id).bind(fields.position)
        .bind(metadata::checked(Metadata::CategoryName, epoch, fields.name_ct)?)
        .execute(&mut *tx).await?.rows_affected();
    if inserted == 0 {
        return Err(keys::conflict("category_exists"));
    }
    audit(
        &mut tx,
        &actor.person,
        "category_created",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Some(Event::ChannelChanged));
    Ok(StatusCode::CREATED)
}

pub async fn update_category(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<CategoryFields>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let fields = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    category_exists(&mut tx, &id).await?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    query("UPDATE categories SET position = ?, name_ct = ? WHERE id = ?")
        .bind(fields.position)
        .bind(metadata::checked(
            Metadata::CategoryName,
            epoch,
            fields.name_ct,
        )?)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &actor.person,
        "category_updated",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Some(Event::ChannelChanged));
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_category(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    category_exists(&mut tx, &id).await?;
    if query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM channels WHERE category_id = ?)")
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?
    {
        return Err(keys::conflict("category_in_use"));
    }
    query("DELETE FROM categories WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &actor.person,
        "category_deleted",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Some(Event::ChannelChanged));
    Ok(StatusCode::NO_CONTENT)
}
