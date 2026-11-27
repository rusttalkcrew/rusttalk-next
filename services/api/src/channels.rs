use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, encode},
    keys::{Event, Kind, Metadata, Part, Scope},
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate, authenticate_read},
    keys, metadata, payload,
    roles::{self, Permission},
    store::{self, audit, audit_details},
};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChannelKind {
    Text,
    Voice,
    /// A text channel shown as one Markdown page: its newest message.
    Page,
    /// A text channel the app groups into posts and their replies.
    Forum,
}

#[derive(Deserialize)]
pub struct ChannelFields {
    category_id: String,
    position: i64,
    name_ct: String,
    topic_ct: Option<String>,
    /// Voice channels only; left as it is when missing, so older clients keep working.
    #[serde(default)]
    audio_kbps: Option<i64>,
    /// The highest bitrate the sender's app offers. Apps up to v0.6.0 send none and stop at 128.
    #[serde(default)]
    audio_kbps_max: Option<i64>,
    /// Voice channels only: the most people in the call, 0 for none. Kept when missing.
    #[serde(default)]
    member_limit: Option<i64>,
    /// Text channels only: seconds between one member's messages, 0 for off. Kept when missing.
    #[serde(default)]
    slow_mode_secs: Option<i64>,
    /// Text, forum and page channels only: whether it may hold mature content. Kept when missing.
    #[serde(default)]
    nsfw: Option<bool>,
}

/// The slow mode choices apps offer, in seconds; 0 is off.
pub const SLOW_MODE_SECS: [i64; 9] = [0, 5, 10, 30, 60, 300, 900, 3600, 21600];

/// Voice bitrate limits in kbps, enforced here and again by the table.
const AUDIO_KBPS: std::ops::RangeInclusive<i64> = 8..=320;
/// The top of the bitrate slider in apps that don't send `audio_kbps_max`.
const OLD_AUDIO_KBPS_MAX: i64 = 128;
/// Voice member limits, 0 for none, enforced here and again by the table.
const MEMBER_LIMIT: std::ops::RangeInclusive<i64> = 0..=99;

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
    audio_kbps: i64,
    member_limit: i64,
    focus_until: Option<i64>,
    slow_mode_secs: i64,
    nsfw: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_kbps: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    member_limit: Option<i64>,
    /// While focus mode is on: when it ends, in unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    focus_until: Option<i64>,
    /// While slow mode is on: the seconds between one member's messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    slow_mode_secs: Option<i64>,
    /// Only while the channel is marked NSFW, so older apps see nothing new.
    #[serde(skip_serializing_if = "Option::is_none")]
    nsfw: Option<bool>,
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

/// Refuse a channel this person can't read as if it didn't exist, so managing channels
/// never reveals or opens up a channel hidden from the manager. The owner reads every channel.
async fn readable(db: &mut SqliteConnection, id: &str, actor: &Actor) -> ApiResult<()> {
    let readable: bool = query_scalar(
        "SELECT EXISTS (SELECT 1 FROM channel_readers r JOIN channels c ON c.id = r.channel_id
                        WHERE r.channel_id = ? AND r.person_id = ? AND c.kind <> 'dm')",
    )
    .bind(id)
    .bind(actor.person.as_slice())
    .fetch_one(db)
    .await?;
    if !readable {
        return Err(ApiError::not_found("channel_not_found"));
    }
    Ok(())
}

const READ: u8 = 1;
pub const WRITE: u8 = 2;
pub const SPEAK: u8 = 4;
pub const VIDEO: u8 = 8;

/// Flags each built-in role gets on a new channel.
const CHANNEL_DEFAULTS: [(&str, u8); 3] = [("owner", 15), ("admin", 15), ("member", 15)];
const PAGE_DEFAULTS: [(&str, u8); 4] = [
    ("owner", READ | WRITE),
    ("admin", READ | WRITE),
    ("moderator", READ | WRITE),
    ("member", READ),
];

pub async fn has_flag(
    db: &mut SqliteConnection,
    channel: &str,
    person: &[u8],
    flag: u8,
) -> ApiResult<bool> {
    Ok(query_scalar(
        "SELECT EXISTS (SELECT 1 FROM channel_permissions
         WHERE channel_id = ? AND (flags & ?) <> 0 AND role_id IN (
             SELECT role_id FROM member_roles WHERE person_id = ?
             UNION ALL SELECT 'owner' FROM server_settings WHERE owner_person_id = ?
         ))",
    )
    .bind(channel)
    .bind(flag)
    .bind(person)
    .bind(person)
    .fetch_one(db)
    .await?)
}

async fn channels(db: &mut SqliteConnection, person: Option<&[u8]>) -> ApiResult<Vec<Channel>> {
    let rows: Vec<ChannelRow> = query_as(
        "SELECT id, CASE WHEN page THEN 'page' WHEN forum THEN 'forum' ELSE kind END AS kind, category_id, position, name_ct, topic_ct,
                CASE WHEN dm_first = ? THEN dm_second ELSE dm_first END AS peer,
                audio_kbps, member_limit, CASE WHEN focus_until > ? THEN focus_until END AS focus_until,
                slow_mode_secs, nsfw
         FROM channels c
         WHERE (? IS NULL AND kind <> 'dm') OR EXISTS
             (SELECT 1 FROM channel_readers r WHERE r.channel_id = c.id AND r.person_id = ?)
         ORDER BY position, id",
    )
    .bind(person)
    .bind(store::now()?)
    .bind(person)
    .bind(person)
    .fetch_all(&mut *db)
    .await?;
    // One query for every channel's flags instead of one per channel (DMs never have rows).
    let permissions: Vec<(String, String, u8)> = query_as(
        "SELECT channel_id, role_id, flags FROM channel_permissions ORDER BY channel_id, role_id",
    )
    .fetch_all(&mut *db)
    .await?;
    let mut by_channel = HashMap::<_, Vec<_>>::new();
    for (channel, role_id, flags) in permissions {
        by_channel
            .entry(channel)
            .or_default()
            .push(RoleFlags { role_id, flags });
    }
    let mut channels = Vec::with_capacity(rows.len());
    for row in rows {
        let flags = by_channel.remove(&row.id).unwrap_or_default();
        let audio_kbps = (row.kind == "voice").then_some(row.audio_kbps);
        let member_limit =
            (row.kind == "voice" && row.member_limit > 0).then_some(row.member_limit);
        channels.push(Channel {
            id: row.id,
            kind: row.kind,
            category_id: row.category_id,
            position: row.position,
            name_ct: metadata::encoded(row.name_ct),
            peer: metadata::encoded(row.peer),
            topic_ct: metadata::encoded(row.topic_ct),
            audio_kbps,
            member_limit,
            focus_until: row.focus_until,
            slow_mode_secs: (row.slow_mode_secs > 0).then_some(row.slow_mode_secs),
            nsfw: row.nsfw.then_some(true),
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
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
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
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    // Only the channels this manager can read, like the catalog.
    let list = channels(&mut tx, Some(actor.person.as_slice())).await?;
    let list = list.into_iter().filter(|c| c.kind != "dm").collect();
    tx.commit().await?;
    Ok(Json(list))
}

pub async fn list_categories(
    State(app): State<App>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<Category>>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
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
    let page = matches!(kind, ChannelKind::Page);
    let forum = matches!(kind, ChannelKind::Forum);
    // A new channel goes last in its category, so it never jumps ahead of a chosen order.
    query("INSERT INTO channels (id, kind, page, forum, category_id, position, name_ct, topic_ct)
           SELECT ?, ?, ?, ?, ?, COALESCE(MAX(position) + 1, 0), ?, ? FROM channels WHERE category_id = ?")
        .bind(&id).bind(match kind { ChannelKind::Voice => "voice", _ => "text" }).bind(page).bind(forum)
        .bind(&fields.category_id)
        .bind(metadata::checked(Metadata::ChannelName, epoch, fields.name_ct)?)
        .bind(metadata::optional(Metadata::ChannelTopic, epoch, fields.topic_ct)?)
        .bind(&fields.category_id)
        .execute(&mut *tx).await?;
    // Everyone may read a page, but only staff may write (edit) it.
    let (defaults, full) = if page {
        (&PAGE_DEFAULTS[..], READ | WRITE)
    } else {
        (&CHANNEL_DEFAULTS[..], READ | WRITE | SPEAK | VIDEO)
    };
    for &(role, flags) in defaults {
        query(
            "INSERT INTO channel_permissions (channel_id, role_id, flags)
               SELECT ?, id, ? FROM roles WHERE id = ?",
        )
        .bind(&id)
        .bind(flags)
        .bind(role)
        .execute(&mut *tx)
        .await?;
    }
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
               SELECT ?, r.id, ? FROM member_roles m JOIN roles r ON m.role_id = r.id
               WHERE m.person_id = ? AND (r.permissions & 64) <> 0 ORDER BY r.rank DESC, r.id LIMIT 1")
            .bind(&id).bind(full).bind(actor.person.as_slice()).execute(&mut *tx).await?;
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
    app.notify(Event::ChannelChanged { part: None });
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
    readable(&mut tx, &id, &actor).await?;
    if let Some(kbps) = fields.audio_kbps {
        if !AUDIO_KBPS.contains(&kbps) {
            return Err(ApiError::bad_request("invalid_audio_kbps"));
        }
        // An app whose slider stops below the stored bitrate clamps it there and sends that on
        // save, so its slider maximum keeps the higher bitrate instead of lowering it.
        let max = fields.audio_kbps_max.unwrap_or(OLD_AUDIO_KBPS_MAX);
        let voice = query(
            "UPDATE channels SET audio_kbps = CASE WHEN ? = ? AND audio_kbps > ? THEN audio_kbps ELSE ? END
             WHERE id = ? AND kind = 'voice'",
        )
        .bind(kbps)
        .bind(max)
        .bind(max)
        .bind(kbps)
        .bind(&id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if voice == 0 {
            return Err(ApiError::bad_request("not_a_voice_channel"));
        }
    }
    if let Some(limit) = fields.member_limit {
        if !MEMBER_LIMIT.contains(&limit) {
            return Err(ApiError::bad_request("invalid_member_limit"));
        }
        let voice = query("UPDATE channels SET member_limit = ? WHERE id = ? AND kind = 'voice'")
            .bind(limit)
            .bind(&id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if voice == 0 {
            return Err(ApiError::bad_request("not_a_voice_channel"));
        }
    }
    if let Some(secs) = fields.slow_mode_secs {
        set_slow_mode(&mut tx, &actor, &id, secs).await?;
    }
    if let Some(nsfw) = fields.nsfw {
        set_nsfw(&mut tx, &actor, &id, nsfw).await?;
    }
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
    app.notify(Event::ChannelChanged {
        part: Some(Part::Channels),
    });
    Ok(StatusCode::NO_CONTENT)
}

/// Set a text channel's slow mode, and audit it when it changed.
async fn set_slow_mode(
    db: &mut SqliteConnection,
    actor: &Actor,
    id: &str,
    secs: i64,
) -> ApiResult<()> {
    if !SLOW_MODE_SECS.contains(&secs) {
        return Err(ApiError::bad_request("invalid_slow_mode"));
    }
    let before: i64 = query_scalar(
        "SELECT slow_mode_secs FROM channels WHERE id = ? AND kind = 'text' AND NOT page",
    )
    .bind(id)
    .fetch_optional(&mut *db)
    .await?
    .ok_or(ApiError::bad_request("not_a_text_channel"))?;
    if before == secs {
        return Ok(());
    }
    query("UPDATE channels SET slow_mode_secs = ? WHERE id = ?")
        .bind(secs)
        .bind(id)
        .execute(&mut *db)
        .await?;
    audit_details(
        db,
        &actor.person,
        "slow_mode_changed",
        None,
        Some(id),
        Some(serde_json::json!({ "before": before, "after": secs })),
        actor.at,
    )
    .await?;
    Ok(())
}

/// Mark a text, forum or page channel NSFW or not, and audit it when it changed.
async fn set_nsfw(db: &mut SqliteConnection, actor: &Actor, id: &str, nsfw: bool) -> ApiResult<()> {
    let before: bool = query_scalar("SELECT nsfw FROM channels WHERE id = ? AND kind = 'text'")
        .bind(id)
        .fetch_optional(&mut *db)
        .await?
        .ok_or(ApiError::bad_request("not_a_text_channel"))?;
    if before == nsfw {
        return Ok(());
    }
    query("UPDATE channels SET nsfw = ? WHERE id = ?")
        .bind(nsfw)
        .bind(id)
        .execute(&mut *db)
        .await?;
    audit_details(
        db,
        &actor.person,
        "nsfw_changed",
        None,
        Some(id),
        Some(serde_json::json!({ "before": before, "after": nsfw })),
        actor.at,
    )
    .await?;
    Ok(())
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
    // Without this, a manager could give a role they hold read access to a hidden channel,
    // and with it every past key of that channel.
    readable(&mut tx, &id, &actor).await?;
    authority.can_set_channel_flags(roles::by_id(&mut tx, &role).await?)?;
    let before = readers(&mut tx, &id).await?;
    let old_flags: i64 =
        query_scalar("SELECT flags FROM channel_permissions WHERE channel_id = ? AND role_id = ?")
            .bind(&id)
            .bind(&role)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(0);
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
    audit_details(
        &mut tx,
        &actor.person,
        "channel_permissions_changed",
        None,
        Some(&id),
        Some(serde_json::json!({ "role_id": role, "before": old_flags, "after": flags })),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    crate::voice::enforce(&app, Some(id));
    app.notify(Event::ChannelChanged { part: None });
    Ok(StatusCode::NO_CONTENT)
}

/// Focus mode lasts at most a day.
const FOCUS_SECONDS: i64 = 24 * 60 * 60;

#[derive(Deserialize)]
pub struct FocusFields {
    /// How long from now focus mode lasts; 0 ends it.
    seconds: i64,
}

/// Start or end focus mode in a channel this person can read.
pub async fn focus(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<FocusFields>, JsonRejection>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let seconds = payload(body)?.seconds;
    if !(0..=FOCUS_SECONDS).contains(&seconds) {
        return Err(ApiError::bad_request("invalid_focus_time"));
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::StartFocus)?;
    let until = (seconds > 0).then_some(actor.at + seconds);
    let changed = query(
        "UPDATE channels SET focus_until = ? WHERE id = ? AND kind = 'voice' AND EXISTS
             (SELECT 1 FROM channel_readers r WHERE r.channel_id = channels.id AND r.person_id = ?)",
    )
    .bind(until)
    .bind(&id)
    .bind(actor.person.as_slice())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 0 {
        // Only voice channels have focus mode; others they can read get a clear refusal.
        let readable: bool = query_scalar(
            "SELECT EXISTS (SELECT 1 FROM channel_readers WHERE channel_id = ? AND person_id = ?)",
        )
        .bind(&id)
        .bind(actor.person.as_slice())
        .fetch_one(&mut *tx)
        .await?;
        return Err(if readable {
            ApiError::bad_request("not_a_voice_channel")
        } else {
            ApiError::not_found("channel_not_found")
        });
    }
    let action = if until.is_some() {
        "focus_started"
    } else {
        "focus_ended"
    };
    audit(&mut tx, &actor.person, action, None, Some(&id), actor.at).await?;
    tx.commit().await?;
    app.notify(Event::ChannelChanged {
        part: Some(Part::Channels),
    });
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
    readable(&mut tx, &id, &actor).await?;
    let kind: String =
        query_scalar("DELETE FROM channels WHERE id = ? AND kind <> 'dm' RETURNING kind")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::not_found("channel_not_found"))?;
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
    crate::voice::close_rooms(&app, id, kind == "voice");
    app.notify(Event::ChannelChanged { part: None });
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
    let inserted = query(
        "INSERT INTO categories (id, position, name_ct)
                          SELECT ?, COALESCE(MAX(position) + 1, 0), ? FROM categories WHERE true
                          ON CONFLICT (id) DO NOTHING",
    )
    .bind(&id)
    .bind(metadata::checked(
        Metadata::CategoryName,
        epoch,
        fields.name_ct,
    )?)
    .execute(&mut *tx)
    .await?
    .rows_affected();
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
    app.notify(Event::ChannelChanged {
        part: Some(Part::Channels),
    });
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
    app.notify(Event::ChannelChanged {
        part: Some(Part::Channels),
    });
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
    app.notify(Event::ChannelChanged {
        part: Some(Part::Channels),
    });
    Ok(StatusCode::NO_CONTENT)
}

/// Largest order body: about 1,400 channel and category IDs.
pub const ORDER_BODY_LIMIT: usize = 64 * 1024;

#[derive(Deserialize)]
pub struct OrderedCategory {
    id: String,
    channels: Vec<String>,
}

#[derive(Deserialize)]
pub struct Order {
    categories: Vec<OrderedCategory>,
}

/// Apply a full new order of every category and every channel this manager can read at once.
pub async fn reorder(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Order>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let order = payload(body)?;
    let mut categories = std::collections::BTreeSet::new();
    let mut channels = std::collections::BTreeSet::new();
    for category in &order.categories {
        decode::<32>(&category.id)?;
        if !categories.insert(category.id.as_str()) {
            return Err(ApiError::bad_request("invalid_order"));
        }
        for channel in &category.channels {
            decode::<32>(channel)?;
            if !channels.insert(channel.as_str()) {
                return Err(ApiError::bad_request("invalid_order"));
            }
        }
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    manage(&mut tx, &actor).await?;
    let known_categories: Vec<String> = query_scalar("SELECT id FROM categories ORDER BY id")
        .fetch_all(&mut *tx)
        .await?;
    // Every channel with whether this manager can read it, in its current order.
    let current: Vec<(String, Option<String>, bool)> = query_as(
        "SELECT id, category_id, EXISTS (SELECT 1 FROM channel_readers r
                                         WHERE r.channel_id = c.id AND r.person_id = ?)
         FROM channels c WHERE kind <> 'dm' ORDER BY position, id",
    )
    .bind(actor.person.as_slice())
    .fetch_all(&mut *tx)
    .await?;
    // The order names exactly the channels this manager can read, as the channel list shows.
    let mut known_channels: Vec<String> = (current.iter())
        .filter(|(_, _, visible)| *visible)
        .map(|(id, _, _)| id.clone())
        .collect();
    known_channels.sort();
    // Both sides are sorted, so equal lists mean exactly the current IDs.
    let same = |given: &std::collections::BTreeSet<&str>, known: &[String]| {
        given.iter().copied().eq(known.iter().map(String::as_str))
    };
    if !same(&categories, &known_categories) || !same(&channels, &known_channels) {
        return Err(keys::conflict("order_changed"));
    }
    for (position, category) in (0_i64..).zip(&order.categories) {
        query("UPDATE categories SET position = ? WHERE id = ?")
            .bind(position)
            .bind(&category.id)
            .execute(&mut *tx)
            .await?;
        // Channels this manager can't see keep their category and their place among the
        // others; each place of a visible channel takes the next wanted one, the rest go last.
        let mut wanted = category.channels.iter();
        let mut list: Vec<&String> = (current.iter())
            .filter(|(_, at, _)| at.as_deref() == Some(category.id.as_str()))
            .filter_map(|(id, _, visible)| if *visible { wanted.next() } else { Some(id) })
            .collect();
        list.extend(wanted);
        for (position, channel) in (0_i64..).zip(list) {
            query("UPDATE channels SET category_id = ?, position = ? WHERE id = ?")
                .bind(&category.id)
                .bind(position)
                .bind(channel)
                .execute(&mut *tx)
                .await?;
        }
    }
    audit(
        &mut tx,
        &actor.person,
        "channels_reordered",
        None,
        None,
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Event::ChannelChanged {
        part: Some(Part::Channels),
    });
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Change, store};
    use rusttalk_core::presence::Update;

    fn id(byte: u8) -> String {
        encode(&[byte; 32])
    }

    fn order(categories: &[(u8, &[u8])]) -> Result<Json<Order>, JsonRejection> {
        Ok(Json(Order {
            categories: categories
                .iter()
                .map(|(category, channels)| OrderedCategory {
                    id: id(*category),
                    channels: channels.iter().map(|channel| id(*channel)).collect(),
                })
                .collect(),
        }))
    }

    #[tokio::test]
    async fn reorder_checks_permission_and_ids_then_saves_and_signals() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = store::open(dir.path()).await?;
        let app = App {
            emoji: crate::emoji::Emoji::open(dir.path(), &db).await?.into(),
            files: crate::files::Files::open(dir.path(), &db).await?.into(),
            db,
            key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32])),
            changes: tokio::sync::broadcast::channel(128).0,
            presence: Default::default(),
            limits: crate::limits::Limits::for_tests(None, true).into(),
            voice: None,
            session_ttl: 3600,
            dm_starts: Default::default(),
            listing_changed: tokio::sync::watch::channel(()).0,
            joins: Default::default(),
        };
        // Person 1 owns the server; person 2 is a member without any role.
        let person = |byte: u8| vec![byte; 32];
        query("INSERT INTO members (person_id, joined_at) VALUES (?, 0), (?, 0)")
            .bind(person(1))
            .bind(person(2))
            .execute(&app.db)
            .await?;
        query("UPDATE server_settings SET owner_person_id = ?")
            .bind(person(1))
            .execute(&app.db)
            .await?;
        for byte in [1_u8, 2] {
            query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, '192.0.2.1', ?, ?)")
                .bind(store::hash(&[byte; 32]).as_slice()).bind(person(byte)).bind(vec![byte])
                .bind(i64::MAX).bind(vec![byte]).execute(&app.db).await?;
        }
        // Categories 10 and 11; channels 20 and 21 in 10, 22 in 11, and a DM that stays out.
        for (category, at) in [(10_u8, 0), (11, 1)] {
            query("INSERT INTO categories (id, position, name_ct) VALUES (?, ?, x'00')")
                .bind(id(category))
                .bind(at)
                .execute(&app.db)
                .await?;
        }
        for (channel, category) in [(20_u8, 10_u8), (21, 10), (22, 11)] {
            query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, 'text', ?, 0, x'00')")
                .bind(id(channel)).bind(id(category)).execute(&app.db).await?;
        }
        query("INSERT INTO channels (id, kind, position, dm_first, dm_second) VALUES ('dm', 'dm', 0, ?, ?)")
            .bind(person(1)).bind(person(2)).execute(&app.db).await?;
        let as_person = |person: u8| {
            let mut headers = HeaderMap::new();
            let token = format!("Bearer {}", encode(&[person; 32]));
            headers.insert("authorization", token.parse().unwrap());
            headers.insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
            headers
        };
        let mut events = app.changes.subscribe();
        let wanted: &[(u8, &[u8])] = &[(11, &[21, 22]), (10, &[20])];
        let refused = |result: ApiResult<StatusCode>| result.unwrap_err().code;

        let denied = reorder(State(app.clone()), as_person(2), order(wanted)).await;
        assert_eq!(refused(denied), "permission_denied");
        for (bad, code) in [
            (&[(11, &[21, 22][..]), (10, &[20, 21])][..], "invalid_order"),
            (&[(11, &[21, 22]), (11, &[20])], "invalid_order"),
            (&[(11, &[21, 22]), (10, &[])], "order_changed"),
            (&[(11, &[21, 22]), (10, &[20, 23])], "order_changed"),
            (&[(11, &[21, 22])], "order_changed"),
        ] {
            let result = reorder(State(app.clone()), as_person(1), order(bad)).await;
            assert_eq!(refused(result), code);
        }
        assert!(events.try_recv().is_err(), "refusals change nothing");

        let saved = reorder(State(app.clone()), as_person(1), order(wanted)).await;
        assert_eq!(saved.unwrap(), StatusCode::NO_CONTENT);
        let rows: Vec<(String, String, i64)> = query_as(
            "SELECT id, category_id, position FROM channels WHERE kind <> 'dm' ORDER BY id",
        )
        .fetch_all(&app.db)
        .await?;
        assert_eq!(
            rows,
            [
                (id(20), id(10), 0),
                (id(21), id(11), 0),
                (id(22), id(11), 1)
            ]
        );
        let categories: Vec<(String, i64)> =
            query_as("SELECT id, position FROM categories ORDER BY id")
                .fetch_all(&app.db)
                .await?;
        assert_eq!(categories, [(id(10), 1), (id(11), 0)]);
        let audited: i64 =
            query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'channels_reordered'")
                .fetch_one(&app.db)
                .await?;
        assert_eq!(audited, 1);
        assert!(matches!(
            events.try_recv(),
            Ok(Change::Update(notice)) if matches!(notice.update, Update::Event(Event::ChannelChanged { .. }))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn widening_the_bitrate_keeps_channels_and_what_hangs_off_them() -> anyhow::Result<()> {
        use sqlx::{Connection, migrate::Migrate};
        let dir = tempfile::tempdir()?;
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.path().join("rusttalk.sqlite3"))
            .create_if_missing(true)
            .foreign_keys(true);
        let mut old = sqlx::SqliteConnection::connect_with(&options).await?;
        old.ensure_migrations_table().await?;
        for migration in sqlx::migrate!().iter().take_while(|m| m.version < 47) {
            old.apply(migration).await?;
        }
        // A voice channel with its key scope and a role's permissions, which cascade from it.
        query("INSERT INTO categories (id, position, name_ct) VALUES ('c', 0, x'00')")
            .execute(&mut old)
            .await?;
        query("INSERT INTO roles (id, kind, rank, permissions) VALUES ('r', 'custom', 5, 1)")
            .execute(&mut old)
            .await?;
        query("INSERT INTO channels (id, kind, category_id, position, name_ct, audio_kbps, member_limit) VALUES ('v', 'voice', 'c', 3, x'01', 128, 5)")
            .execute(&mut old)
            .await?;
        query("INSERT INTO key_scopes (id, kind, channel_id) VALUES ('v', 'channel', 'v')")
            .execute(&mut old)
            .await?;
        query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES ('v', 'r', 3)")
            .execute(&mut old)
            .await?;
        assert!(
            query("UPDATE channels SET audio_kbps = 320")
                .execute(&mut old)
                .await
                .is_err()
        );
        old.close().await?;

        let db = store::open(dir.path()).await?;
        let channel: (i64, i64, i64) =
            query_as("SELECT audio_kbps, member_limit, position FROM channels WHERE id = 'v'")
                .fetch_one(&db)
                .await?;
        assert_eq!(channel, (128, 5, 3));
        let kept: (i64, i64) = query_as(
            "SELECT (SELECT COUNT(*) FROM key_scopes), (SELECT COUNT(*) FROM channel_permissions)",
        )
        .fetch_one(&db)
        .await?;
        assert_eq!(kept, (1, 1));
        query("UPDATE channels SET audio_kbps = 320")
            .execute(&db)
            .await?;
        for refused in [7, 321] {
            let update = query("UPDATE channels SET audio_kbps = ?").bind(refused);
            assert!(update.execute(&db).await.is_err(), "{refused}");
        }
        query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES ('w', 'voice', 'c', 4, x'02')")
            .execute(&db)
            .await?;
        let fresh: i64 = query_scalar("SELECT audio_kbps FROM channels WHERE id = 'w'")
            .fetch_one(&db)
            .await?;
        assert_eq!(fresh, 64);
        Ok(())
    }

    #[tokio::test]
    async fn audio_bitrate_is_a_managers_voice_only_setting_in_range() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = store::open(dir.path()).await?;
        let app = App {
            emoji: crate::emoji::Emoji::open(dir.path(), &db).await?.into(),
            files: crate::files::Files::open(dir.path(), &db).await?.into(),
            db,
            key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32])),
            changes: tokio::sync::broadcast::channel(128).0,
            presence: Default::default(),
            limits: crate::limits::Limits::for_tests(None, true).into(),
            voice: None,
            session_ttl: 3600,
            dm_starts: Default::default(),
            listing_changed: tokio::sync::watch::channel(()).0,
            joins: Default::default(),
        };
        // Person 1 owns the server; person 2 is a member without any role.
        let person = |byte: u8| vec![byte; 32];
        query("INSERT INTO members (person_id, joined_at) VALUES (?, 0), (?, 0)")
            .bind(person(1))
            .bind(person(2))
            .execute(&app.db)
            .await?;
        query("UPDATE server_settings SET owner_person_id = ?")
            .bind(person(1))
            .execute(&app.db)
            .await?;
        for byte in [1_u8, 2] {
            query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, '192.0.2.1', ?, ?)")
                .bind(store::hash(&[byte; 32]).as_slice()).bind(person(byte)).bind(vec![byte])
                .bind(i64::MAX).bind(vec![byte]).execute(&app.db).await?;
        }
        query("INSERT INTO categories (id, position, name_ct) VALUES (?, 0, x'00')")
            .bind(id(10))
            .execute(&app.db)
            .await?;
        for (channel, kind, page, forum) in [
            (20_u8, "text", false, false),
            (21, "voice", false, false),
            (22, "text", true, false),
            (23, "text", false, true),
        ] {
            query("INSERT INTO channels (id, kind, page, forum, category_id, position, name_ct) VALUES (?, ?, ?, ?, ?, 0, x'00')")
                .bind(id(channel)).bind(kind).bind(page).bind(forum).bind(id(10)).execute(&app.db).await?;
        }
        let as_person = |person: u8| {
            let mut headers = HeaderMap::new();
            let token = format!("Bearer {}", encode(&[person; 32]));
            headers.insert("authorization", token.parse().unwrap());
            headers.insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
            headers
        };
        let kbps = |value: Option<i64>| {
            // The name is replaced only when it validates as sealed metadata, so reuse the stored one.
            Ok(Json(ChannelFields {
                category_id: id(10),
                position: 0,
                name_ct: String::new(),
                topic_ct: None,
                audio_kbps: value,
                audio_kbps_max: None,
                member_limit: None,
                slow_mode_secs: None,
                nsfw: None,
            }))
        };
        let stored = || async {
            anyhow::Ok(
                query_scalar::<_, i64>("SELECT audio_kbps FROM channels WHERE id = ?")
                    .bind(id(21))
                    .fetch_one(&app.db)
                    .await?,
            )
        };
        assert_eq!(stored().await?, 64, "new channels start at 64 kbps");

        // A member without Manage channels cannot change it.
        let denied = update_channel(
            State(app.clone()),
            as_person(2),
            Path(id(21)),
            kbps(Some(96)),
        )
        .await;
        assert_eq!(denied.unwrap_err().code, "permission_denied");
        assert_eq!(stored().await?, 64);

        // Out-of-range values and text channels are refused before anything changes.
        for (channel, value, code) in [
            (21_u8, 7, "invalid_audio_kbps"),
            (21, 321, "invalid_audio_kbps"),
            (20, 96, "not_a_voice_channel"),
        ] {
            let result = update_channel(
                State(app.clone()),
                as_person(1),
                Path(id(channel)),
                kbps(Some(value)),
            )
            .await;
            assert_eq!(result.unwrap_err().code, code);
        }
        assert_eq!(stored().await?, 64);

        // Readers see it in the listing, for voice channels only; pages and forums list as their own kinds.
        let mut tx = app.db.begin().await?;
        let listed = channels(&mut tx, Some(person(1).as_slice())).await?;
        let shown: Vec<_> = listed
            .iter()
            .map(|c| (c.kind.as_str(), c.audio_kbps))
            .collect();
        assert_eq!(
            shown,
            [
                ("text", None),
                ("voice", Some(64)),
                ("page", None),
                ("forum", None)
            ]
        );
        drop(tx);
        Ok(())
    }

    #[tokio::test]
    async fn focus_needs_its_permission_and_the_channel_and_lasts_up_to_a_day() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let db = store::open(dir.path()).await?;
        let app = App {
            emoji: crate::emoji::Emoji::open(dir.path(), &db).await?.into(),
            files: crate::files::Files::open(dir.path(), &db).await?.into(),
            db,
            key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32])),
            changes: tokio::sync::broadcast::channel(128).0,
            presence: Default::default(),
            limits: crate::limits::Limits::for_tests(None, true).into(),
            voice: None,
            session_ttl: 3600,
            dm_starts: Default::default(),
            listing_changed: tokio::sync::watch::channel(()).0,
            joins: Default::default(),
        };
        // Person 1 is a Member; person 2 has no role and so no `start_focus`.
        let person = |byte: u8| vec![byte; 32];
        query("INSERT INTO members (person_id, joined_at) VALUES (?, 0), (?, 0)")
            .bind(person(1))
            .bind(person(2))
            .execute(&app.db)
            .await?;
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'member')")
            .bind(person(1))
            .execute(&app.db)
            .await?;
        for byte in [1_u8, 2] {
            query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, '192.0.2.1', ?, ?)")
                .bind(store::hash(&[byte; 32]).as_slice()).bind(person(byte)).bind(vec![byte])
                .bind(i64::MAX).bind(vec![byte]).execute(&app.db).await?;
        }
        query("INSERT INTO categories (id, position, name_ct) VALUES (?, 0, x'00')")
            .bind(id(10))
            .execute(&app.db)
            .await?;
        // Members read voice channel 20 and text channel 22, but not voice channel 21.
        for (channel, kind) in [(20_u8, "voice"), (21, "voice"), (22, "text")] {
            query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, ?, ?, 0, x'00')")
                .bind(id(channel)).bind(kind).bind(id(10)).execute(&app.db).await?;
        }
        for channel in [20_u8, 22] {
            query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES (?, 'member', 15)")
                .bind(id(channel)).execute(&app.db).await?;
        }
        let as_person = |person: u8| {
            let mut headers = HeaderMap::new();
            let token = format!("Bearer {}", encode(&[person; 32]));
            headers.insert("authorization", token.parse().unwrap());
            headers.insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
            headers
        };
        let set = |person: u8, channel: u8, seconds: i64| {
            focus(
                State(app.clone()),
                as_person(person),
                Path(id(channel)),
                Ok(Json(FocusFields { seconds })),
            )
        };
        let refused = |result: ApiResult<StatusCode>| result.unwrap_err().code;
        let until = || async {
            let mut db = app.db.acquire().await?;
            let shown = channels(&mut db, Some(&person(1))).await?;
            anyhow::Ok(
                shown
                    .into_iter()
                    .find(|c| c.id == id(20))
                    .unwrap()
                    .focus_until,
            )
        };

        assert_eq!(refused(set(2, 20, 600).await), "permission_denied");
        assert_eq!(refused(set(1, 21, 600).await), "channel_not_found");
        assert_eq!(refused(set(1, 22, 600).await), "not_a_voice_channel");
        for seconds in [-1, FOCUS_SECONDS + 1] {
            assert_eq!(refused(set(1, 20, seconds).await), "invalid_focus_time");
        }
        let mut events = app.changes.subscribe();
        assert_eq!(
            set(1, 20, FOCUS_SECONDS).await.unwrap(),
            StatusCode::NO_CONTENT
        );
        let end = until().await?.expect("focus mode is on");
        assert!((end - store::now()? - FOCUS_SECONDS).abs() <= 5);
        assert!(matches!(
            events.try_recv(),
            Ok(Change::Update(notice)) if matches!(notice.update, Update::Event(Event::ChannelChanged { .. }))
        ));
        // Once the end has passed, the catalog no longer shows it.
        query("UPDATE channels SET focus_until = ? WHERE id = ?")
            .bind(store::now()? - 1)
            .bind(id(20))
            .execute(&app.db)
            .await?;
        assert_eq!(until().await?, None);
        assert_eq!(set(1, 20, 600).await.unwrap(), StatusCode::NO_CONTENT);
        assert!(until().await?.is_some());
        assert_eq!(set(1, 20, 0).await.unwrap(), StatusCode::NO_CONTENT);
        assert_eq!(until().await?, None);
        let audited: i64 = query_scalar(
            "SELECT COUNT(*) FROM audit_log WHERE action IN ('focus_started', 'focus_ended')",
        )
        .fetch_one(&app.db)
        .await?;
        assert_eq!(audited, 3);
        Ok(())
    }

    #[tokio::test]
    async fn catalog_lists_each_channels_flags_by_role_and_none_for_dms() -> anyhow::Result<()> {
        use crate::testing::{app, as_person, person};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'member')")
            .bind(person(2))
            .execute(&app.db)
            .await?;
        query("INSERT INTO categories (id, position, name_ct) VALUES (?, 0, x'00')")
            .bind(id(10))
            .execute(&app.db)
            .await?;
        // Channels 20..=23 in reverse position order; 23 has no rows. Rows go in unsorted.
        for (channel, kind) in [(20_u8, "text"), (21, "voice"), (22, "text"), (23, "text")] {
            query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, ?, ?, ?, x'00')")
                .bind(id(channel))
                .bind(kind)
                .bind(id(10))
                .bind(30 - i64::from(channel))
                .execute(&app.db)
                .await?;
        }
        for (channel, role, flags) in [
            (22_u8, "moderator", 3_u8),
            (20, "owner", 15),
            (22, "member", 1),
            (21, "member", 13),
            (22, "admin", 15),
            (20, "member", 3),
            (21, "admin", 5),
            (22, "owner", 7),
        ] {
            query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES (?, ?, ?)")
                .bind(id(channel))
                .bind(role)
                .bind(flags)
                .execute(&app.db)
                .await?;
        }
        query("INSERT INTO channels (id, kind, position, dm_first, dm_second) VALUES ('dm', 'dm', 0, ?, ?)")
            .bind(person(1)).bind(person(2)).execute(&app.db).await?;

        // Each channel's flags as JSON, in catalog order (position, then id), roles sorted.
        let entry = |channel: String, flags: &[(&str, u8)]| {
            let flags: Vec<_> = flags
                .iter()
                .map(|(role, flags)| serde_json::json!({ "role_id": role, "flags": flags }))
                .collect();
            serde_json::json!([channel, flags])
        };
        let readable = [
            entry("dm".into(), &[]),
            entry(
                id(22),
                &[("admin", 15), ("member", 1), ("moderator", 3), ("owner", 7)],
            ),
            entry(id(21), &[("admin", 5), ("member", 13)]),
            entry(id(20), &[("member", 3), ("owner", 15)]),
        ];
        // The owner reads every channel, so also 23 (no rows); the member reads 20..=22 and the DM.
        let mut owner = readable.to_vec();
        owner.insert(1, entry(id(23), &[]));
        for (who, expected) in [(1_u8, owner), (2, readable.to_vec())] {
            let Json(catalog) = catalog(State(app.clone()), as_person(who)).await?;
            let listed = serde_json::to_value(&catalog)?;
            let shown: Vec<_> = listed["channels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| serde_json::json!([c["id"], c["flags"]]))
                .collect();
            assert_eq!(shown, expected, "person {who}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn managers_only_manage_channels_they_can_read() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = store::open(dir.path()).await?;
        let app = App {
            emoji: crate::emoji::Emoji::open(dir.path(), &db).await?.into(),
            files: crate::files::Files::open(dir.path(), &db).await?.into(),
            db,
            key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32])),
            changes: tokio::sync::broadcast::channel(128).0,
            presence: Default::default(),
            limits: crate::limits::Limits::for_tests(None, true).into(),
            voice: None,
            session_ttl: 3600,
            dm_starts: Default::default(),
            listing_changed: tokio::sync::watch::channel(()).0,
            joins: Default::default(),
        };
        // Person 1 owns the server; person 2 holds a custom role with Manage channels,
        // ranked above Member and below Admin.
        let person = |byte: u8| vec![byte; 32];
        query("INSERT INTO members (person_id, joined_at) VALUES (?, 0), (?, 0)")
            .bind(person(1))
            .bind(person(2))
            .execute(&app.db)
            .await?;
        query("UPDATE server_settings SET owner_person_id = ?")
            .bind(person(1))
            .execute(&app.db)
            .await?;
        query(
            "INSERT INTO roles (id, kind, rank, permissions) VALUES ('manager', 'custom', 60, 64)",
        )
        .execute(&app.db)
        .await?;
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'manager')")
            .bind(person(2))
            .execute(&app.db)
            .await?;
        for byte in [1_u8, 2] {
            query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, '192.0.2.1', ?, ?)")
                .bind(store::hash(&[byte; 32]).as_slice()).bind(person(byte)).bind(vec![byte])
                .bind(i64::MAX).bind(vec![byte]).execute(&app.db).await?;
        }
        query("INSERT INTO categories (id, position, name_ct) VALUES (?, 0, x'00')")
            .bind(id(10))
            .execute(&app.db)
            .await?;
        // Channels 20 and 22 are open to the manager's role; 21 only to Admins.
        for (channel, at) in [(20_u8, 0), (21, 1), (22, 2)] {
            query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, 'text', ?, ?, x'00')")
                .bind(id(channel)).bind(id(10)).bind(at).execute(&app.db).await?;
            query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES (?, 'admin', 15)")
                .bind(id(channel))
                .execute(&app.db)
                .await?;
        }
        for channel in [20_u8, 22] {
            query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES (?, 'manager', 15)")
                .bind(id(channel))
                .execute(&app.db)
                .await?;
        }
        let as_person = |person: u8| {
            let mut headers = HeaderMap::new();
            let token = format!("Bearer {}", encode(&[person; 32]));
            headers.insert("authorization", token.parse().unwrap());
            headers.insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
            headers
        };
        let code = |result: ApiResult<StatusCode>| result.unwrap_err().code;
        let listed = |who: u8| {
            let app = app.clone();
            async move {
                let Json(list) = list_channels(State(app), as_person(who)).await.unwrap();
                list.into_iter().map(|c| c.id).collect::<Vec<_>>()
            }
        };
        let flags = |channel: u8| {
            let app = app.clone();
            async move {
                query_scalar::<_, u8>(
                    "SELECT flags FROM channel_permissions WHERE channel_id = ? AND role_id = 'member'",
                )
                .bind(id(channel))
                .fetch_optional(&app.db)
                .await
                .unwrap()
            }
        };
        let set = |who: u8, channel: u8, value: u8| {
            permissions(
                State(app.clone()),
                as_person(who),
                Path((id(channel), "member".to_owned())),
                Ok(Json(PermissionFields { flags: value })),
            )
        };

        // The list shows the manager only what they can read; the owner sees everything.
        assert_eq!(listed(2).await, [id(20), id(22)]);
        assert_eq!(listed(1).await, [id(20), id(21), id(22)]);

        // A hidden channel can't be opened up, as if it didn't exist; a readable one can.
        assert_eq!(code(set(2, 21, 1).await), "channel_not_found");
        assert_eq!(flags(21).await, None);
        assert_eq!(set(2, 20, 1).await?, StatusCode::NO_CONTENT);
        assert_eq!(flags(20).await, Some(1));
        assert_eq!(set(1, 21, 1).await?, StatusCode::NO_CONTENT);
        assert_eq!(flags(21).await, Some(1));
        assert_eq!(set(1, 21, 0).await?, StatusCode::NO_CONTENT);
        let audited: Vec<(String, String)> = query_as(
            "SELECT target_id, details FROM audit_log
             WHERE action = 'channel_permissions_changed' ORDER BY id",
        )
        .fetch_all(&app.db)
        .await?;
        let entry = |channel: u8, before: u8, after: u8| {
            let details =
                serde_json::json!({ "role_id": "member", "before": before, "after": after });
            (id(channel), details.to_string())
        };
        assert_eq!(audited, [entry(20, 0, 1), entry(21, 0, 1), entry(21, 1, 0)]);

        // Renaming: the hidden channel is refused before anything else; a readable one
        // gets as far as the space key, which this test server has none of.
        let rename = |who: u8, channel: u8| {
            update_channel(
                State(app.clone()),
                as_person(who),
                Path(id(channel)),
                Ok(Json(ChannelFields {
                    category_id: id(10),
                    position: 0,
                    name_ct: String::new(),
                    topic_ct: None,
                    audio_kbps: None,
                    audio_kbps_max: None,
                    member_limit: None,
                    slow_mode_secs: None,
                    nsfw: None,
                })),
            )
        };
        assert_eq!(code(rename(2, 21).await), "channel_not_found");
        assert_eq!(code(rename(2, 20).await), "space_key_unavailable");
        assert_eq!(code(rename(1, 21).await), "space_key_unavailable");

        // Reordering names only readable channels; hidden ones keep their place.
        let hidden_too: &[(u8, &[u8])] = &[(10, &[22, 21, 20])];
        let result = reorder(State(app.clone()), as_person(2), order(hidden_too)).await;
        assert_eq!(code(result), "order_changed");
        let visible: &[(u8, &[u8])] = &[(10, &[22, 20])];
        let saved = reorder(State(app.clone()), as_person(2), order(visible)).await;
        assert_eq!(saved?, StatusCode::NO_CONTENT);
        assert_eq!(listed(1).await, [id(22), id(21), id(20)]);

        // Deleting: refused for the hidden channel, allowed for a readable one and the owner.
        let delete = |who: u8, channel: u8| {
            delete_channel(State(app.clone()), as_person(who), Path(id(channel)))
        };
        assert_eq!(code(delete(2, 21).await), "channel_not_found");
        assert_eq!(listed(1).await.len(), 3);
        assert_eq!(delete(2, 20).await?, StatusCode::NO_CONTENT);
        assert_eq!(delete(1, 21).await?, StatusCode::NO_CONTENT);
        assert_eq!(listed(1).await, [id(22)]);
        Ok(())
    }

    #[tokio::test]
    async fn slow_mode_is_a_managers_text_channel_setting_and_audited() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = crate::testing::app(dir.path(), 2).await?;
        query("INSERT INTO categories (id, position, name_ct) VALUES (?, 0, x'00')")
            .bind(id(10))
            .execute(&app.db)
            .await?;
        for (channel, kind, page) in [
            (20_u8, "text", false),
            (21, "voice", false),
            (22, "text", true),
        ] {
            query("INSERT INTO channels (id, kind, page, category_id, position, name_ct) VALUES (?, ?, ?, ?, 0, x'00')")
                .bind(id(channel)).bind(kind).bind(page).bind(id(10)).execute(&app.db).await?;
            query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES (?, 'owner', 15), (?, 'member', 15)")
                .bind(id(channel)).bind(id(channel)).execute(&app.db).await?;
        }
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'member')")
            .bind(crate::testing::person(2))
            .execute(&app.db)
            .await?;
        let update = |who: u8, channel: u8, secs: i64| {
            update_channel(
                State(app.clone()),
                crate::testing::as_person(who),
                Path(id(channel)),
                Ok(Json(ChannelFields {
                    category_id: id(10),
                    position: 0,
                    name_ct: String::new(),
                    topic_ct: None,
                    audio_kbps: None,
                    audio_kbps_max: None,
                    member_limit: None,
                    slow_mode_secs: Some(secs),
                    nsfw: None,
                })),
            )
        };
        let code = |result: ApiResult<StatusCode>| result.unwrap_err().code;
        assert_eq!(code(update(2, 20, 30).await), "permission_denied");
        assert_eq!(code(update(1, 20, 7).await), "invalid_slow_mode");
        assert_eq!(code(update(1, 21, 30).await), "not_a_text_channel");
        assert_eq!(code(update(1, 22, 30).await), "not_a_text_channel");

        // Saved and audited with what changed; saving the same value again logs nothing.
        let mut tx = app.db.begin().await?;
        let owner = crate::auth::Actor {
            person: [1; 32],
            device: vec![1],
            ip: "192.0.2.1".into(),
            at: 50,
            session_id: vec![1],
            expires_at: i64::MAX,
        };
        set_slow_mode(&mut tx, &owner, &id(20), 300).await?;
        set_slow_mode(&mut tx, &owner, &id(20), 300).await?;
        let listed = channels(&mut tx, Some(crate::testing::person(1).as_slice())).await?;
        let shown: Vec<_> = listed.iter().map(|c| c.slow_mode_secs).collect();
        assert_eq!(shown, [Some(300), None, None]);
        let audited: Vec<(String, String, String)> =
            query_as("SELECT action, target_id, details FROM audit_log")
                .fetch_all(&mut *tx)
                .await?;
        let details = serde_json::json!({ "before": 0, "after": 300 }).to_string();
        assert_eq!(audited, [("slow_mode_changed".into(), id(20), details)]);
        Ok(())
    }

    #[tokio::test]
    async fn nsfw_is_a_managers_text_channel_flag_and_audited() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = crate::testing::app(dir.path(), 2).await?;
        query("INSERT INTO categories (id, position, name_ct) VALUES (?, 0, x'00')")
            .bind(id(10))
            .execute(&app.db)
            .await?;
        for (channel, kind, page) in [
            (20_u8, "text", false),
            (21, "voice", false),
            (22, "text", true),
        ] {
            query("INSERT INTO channels (id, kind, page, category_id, position, name_ct) VALUES (?, ?, ?, ?, 0, x'00')")
                .bind(id(channel)).bind(kind).bind(page).bind(id(10)).execute(&app.db).await?;
            query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES (?, 'owner', 15), (?, 'member', 15)")
                .bind(id(channel)).bind(id(channel)).execute(&app.db).await?;
        }
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'member')")
            .bind(crate::testing::person(2))
            .execute(&app.db)
            .await?;
        let update = |who: u8, channel: u8, nsfw: bool| {
            update_channel(
                State(app.clone()),
                crate::testing::as_person(who),
                Path(id(channel)),
                Ok(Json(ChannelFields {
                    category_id: id(10),
                    position: 0,
                    name_ct: String::new(),
                    topic_ct: None,
                    audio_kbps: None,
                    audio_kbps_max: None,
                    member_limit: None,
                    slow_mode_secs: None,
                    nsfw: Some(nsfw),
                })),
            )
        };
        let code = |result: ApiResult<StatusCode>| result.unwrap_err().code;
        assert_eq!(code(update(2, 20, true).await), "permission_denied");
        assert_eq!(code(update(1, 21, true).await), "not_a_text_channel");

        // Saved and audited with what changed; saving the same value again logs nothing.
        // Pages may be marked too; only marked channels say so in the list.
        let mut tx = app.db.begin().await?;
        let owner = crate::auth::Actor {
            person: [1; 32],
            device: vec![1],
            ip: "192.0.2.1".into(),
            at: 50,
            session_id: vec![1],
            expires_at: i64::MAX,
        };
        set_nsfw(&mut tx, &owner, &id(20), true).await?;
        set_nsfw(&mut tx, &owner, &id(20), true).await?;
        set_nsfw(&mut tx, &owner, &id(22), true).await?;
        set_nsfw(&mut tx, &owner, &id(22), false).await?;
        let listed = channels(&mut tx, Some(crate::testing::person(1).as_slice())).await?;
        let shown: Vec<_> = listed.iter().map(|c| c.nsfw).collect();
        assert_eq!(shown, [Some(true), None, None]);
        let json = serde_json::to_value(&listed)?;
        assert!(
            json[1].get("nsfw").is_none(),
            "unmarked channels leave it out"
        );
        let audited: Vec<(String, String, String)> =
            query_as("SELECT action, target_id, details FROM audit_log ORDER BY rowid")
                .fetch_all(&mut *tx)
                .await?;
        let details = |before: bool, after: bool| {
            serde_json::json!({ "before": before, "after": after }).to_string()
        };
        assert_eq!(
            audited,
            [
                ("nsfw_changed".into(), id(20), details(false, true)),
                ("nsfw_changed".into(), id(22), details(false, true)),
                ("nsfw_changed".into(), id(22), details(true, false)),
            ]
        );
        Ok(())
    }
}
