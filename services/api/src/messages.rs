use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
};
use ed25519_dalek::VerifyingKey;
use rusttalk_core::{
    auth::{decode, decode_bytes, encode},
    keys::{Event, Kind, Scope},
    messages::{MAX_MESSAGE_BYTES, MessageContext, MessageRecord},
};
use serde::Deserialize;
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate},
    channels, keys, payload,
    roles::{self, Permission},
    store::audit,
};

/// JSON for the largest envelope (unpadded base64url) plus 512 bytes for the ID, epoch and keys.
pub const BODY_LIMIT: usize = (MAX_MESSAGE_BYTES * 4).div_ceil(3) + 512;
const RETAIN: i64 = 10_000;
const PAGE_LIMIT: i64 = 100;

#[derive(Deserialize)]
pub struct NewMessage {
    id: String,
    epoch: i64,
    envelope: String,
}

#[derive(Deserialize)]
pub struct Page {
    before: Option<String>,
    limit: Option<i64>,
}

#[derive(FromRow)]
struct Row {
    id: String,
    epoch: i64,
    sender_person: Vec<u8>,
    sender_device: Vec<u8>,
    at: i64,
    size: u32,
    envelope: Vec<u8>,
}

impl TryFrom<Row> for MessageRecord {
    type Error = ApiError;

    fn try_from(row: Row) -> ApiResult<Self> {
        let invalid = |_| ApiError::internal("invalid_stored_sender");
        Ok(Self {
            id: decode(&row.id).map_err(|_| ApiError::internal("invalid_stored_message"))?,
            epoch: row.epoch,
            person: row.sender_person.try_into().map_err(invalid)?,
            sender_device: row.sender_device.try_into().map_err(invalid)?,
            at: row.at,
            size: row.size as usize,
            envelope: encode(&row.envelope),
        })
    }
}

pub async fn chat_kind(
    db: &mut SqliteConnection,
    actor: &Actor,
    channel: &str,
) -> ApiResult<String> {
    decode::<32>(channel)?;
    readable_kind(db, channel, &actor.person).await
}

pub async fn readable_kind(
    db: &mut SqliteConnection,
    channel: &str,
    person: &[u8],
) -> ApiResult<String> {
    query_scalar(
        "SELECT c.kind FROM channels c JOIN channel_readers r ON r.channel_id = c.id
         WHERE c.id = ? AND r.person_id = ?",
    )
    .bind(channel)
    .bind(person)
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::forbidden("channel_read_denied"))
}

pub async fn require_write(
    db: &mut SqliteConnection,
    actor: &Actor,
    channel: &str,
) -> ApiResult<()> {
    decode::<32>(channel)?;
    require_writer(db, channel, &actor.person).await
}

pub async fn require_writer(
    db: &mut SqliteConnection,
    channel: &str,
    person: &[u8],
) -> ApiResult<()> {
    let kind = readable_kind(db, channel, person).await?;
    if !can_write(db, channel, person, &kind).await? {
        return Err(ApiError::forbidden("channel_write_denied"));
    }
    Ok(())
}

pub async fn can_write(
    db: &mut SqliteConnection,
    channel: &str,
    person: &[u8],
    kind: &str,
) -> ApiResult<bool> {
    Ok(kind == "dm" || channels::has_flag(db, channel, person, channels::WRITE).await?)
}

pub async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    body: Result<Json<NewMessage>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<MessageRecord>)> {
    let NewMessage {
        id,
        epoch,
        envelope,
    } = payload(body)?;
    let channel_id = decode(&channel)?;
    let message_id = decode(&id)?;
    let envelope_bytes = decode_bytes(&envelope)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    require_write(&mut tx, &actor, &channel).await?;
    let scope = Scope {
        kind: Kind::Channel,
        id: channel_id,
    };
    let (current, rotate) = keys::read_epoch(&mut tx, &actor, scope).await?;
    if rotate || epoch != current {
        return Err(keys::conflict("epoch_changed"));
    }
    let device = actor
        .device
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::internal("invalid_stored_device"))?;
    let verification_key = VerifyingKey::from_bytes(&device)
        .map_err(|_| ApiError::internal("invalid_stored_device"))?;
    let context = MessageContext {
        server_id: keys::space(&app).id,
        channel_id,
        epoch,
        id: message_id,
        sender_device: device,
        person: actor.person,
    };
    context
        .verify(&verification_key, &envelope_bytes)
        .map_err(|_| ApiError::bad_request("invalid_message"))?;

    let certificate: Vec<u8> = query_scalar(
        "SELECT certificate FROM member_devices WHERE device_id = ? AND person_id = ?",
    )
    .bind(&actor.device)
    .bind(actor.person.as_slice())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::forbidden("device_certificate_required"))?;
    query(
        "INSERT INTO message_senders (device_id, person_id, certificate) VALUES (?, ?, ?)
         ON CONFLICT (device_id, person_id) DO NOTHING",
    )
    .bind(&actor.device)
    .bind(actor.person.as_slice())
    .bind(certificate)
    .execute(&mut *tx)
    .await?;
    reserve(&mut tx, &id).await?;
    let size = envelope_bytes.len();
    query(
        "INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&channel)
    .bind(epoch)
    .bind(&actor.device)
    .bind(actor.person.as_slice())
    .bind(actor.at)
    .bind(size as i64)
    .bind(envelope_bytes)
    .execute(&mut *tx)
    .await?;
    let dropped = prune(&mut tx, &channel).await?;
    tx.commit().await?;
    for id in dropped {
        app.notify(Some(Event::MessageDeleted {
            channel: channel.clone(),
            id,
        }));
    }
    app.notify(Some(Event::MessageCreated { channel, id }));
    Ok((
        StatusCode::CREATED,
        Json(MessageRecord {
            id: message_id,
            epoch,
            person: actor.person,
            sender_device: device,
            at: actor.at,
            size,
            envelope,
        }),
    ))
}

async fn reserve(db: &mut SqliteConnection, id: &str) -> ApiResult<()> {
    let fresh = query("INSERT INTO message_ids (id) VALUES (?) ON CONFLICT (id) DO NOTHING")
        .bind(id)
        .execute(db)
        .await?
        .rows_affected();
    if fresh == 0 {
        return Err(keys::conflict("message_exists"));
    }
    Ok(())
}

async fn prune(db: &mut SqliteConnection, channel: &str) -> ApiResult<Vec<String>> {
    Ok(query_scalar(
        "DELETE FROM messages WHERE channel_id = ? AND sequence <= (
             SELECT sequence FROM messages WHERE channel_id = ? ORDER BY sequence DESC LIMIT 1 OFFSET ?
         ) RETURNING id",
    )
    .bind(channel)
    .bind(channel)
    .bind(RETAIN)
    .fetch_all(db)
    .await?)
}

async fn sequence(db: &mut SqliteConnection, channel: &str, id: &str) -> ApiResult<i64> {
    decode::<32>(id)?;
    query_scalar("SELECT sequence FROM messages WHERE channel_id = ? AND id = ?")
        .bind(channel)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(ApiError::not_found("message_not_found"))
}

async fn records(
    db: &mut SqliteConnection,
    channel: &str,
    through: i64,
    limit: i64,
) -> ApiResult<Vec<MessageRecord>> {
    let rows: Vec<Row> = query_as(
        "SELECT m.id, m.epoch, m.sender_person, m.sender_device, m.at, m.size, m.envelope
         FROM messages m
         WHERE m.channel_id = ? AND m.sequence <= ? ORDER BY m.sequence DESC LIMIT ?",
    )
    .bind(channel)
    .bind(through)
    .bind(limit)
    .fetch_all(db)
    .await?;
    rows.into_iter().map(MessageRecord::try_from).collect()
}

pub async fn list(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    page: Result<Query<Page>, QueryRejection>,
) -> ApiResult<Json<Vec<MessageRecord>>> {
    let Query(page) = page.map_err(|_| ApiError::bad_request("invalid_query"))?;
    let limit = page.limit.unwrap_or(50);
    if !(1..=PAGE_LIMIT).contains(&limit) {
        return Err(ApiError::bad_request("invalid_query"));
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    chat_kind(&mut tx, &actor, &channel).await?;
    let through = match page.before {
        Some(id) => sequence(&mut tx, &channel, &id).await? - 1,
        None => i64::MAX,
    };
    let list = records(&mut tx, &channel, through, limit).await?;
    tx.commit().await?;
    Ok(Json(list))
}

pub async fn get(
    State(app): State<App>,
    headers: HeaderMap,
    Path((channel, id)): Path<(String, String)>,
) -> ApiResult<Json<MessageRecord>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    chat_kind(&mut tx, &actor, &channel).await?;
    let through = sequence(&mut tx, &channel, &id).await?;
    let record = records(&mut tx, &channel, through, 1)
        .await?
        .pop()
        .ok_or(ApiError::not_found("message_not_found"))?;
    tx.commit().await?;
    Ok(Json(record))
}

pub async fn device(
    State(app): State<App>,
    headers: HeaderMap,
    Path((channel, device, person)): Path<(String, String, String)>,
) -> ApiResult<Json<String>> {
    let device = decode::<32>(&device)?;
    let person = decode::<32>(&person)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    chat_kind(&mut tx, &actor, &channel).await?;
    let certificate: Vec<u8> = query_scalar(
        "SELECT s.certificate FROM message_senders s
         WHERE s.device_id = ? AND s.person_id = ?
           AND EXISTS (SELECT 1 FROM messages m WHERE m.channel_id = ?
             AND m.sender_device = s.device_id AND m.sender_person = s.person_id)",
    )
    .bind(device.as_slice())
    .bind(person.as_slice())
    .bind(&channel)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::not_found("message_device_not_found"))?;
    tx.commit().await?;
    Ok(Json(encode(&certificate)))
}

pub async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    Path((channel, id)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let kind = chat_kind(&mut tx, &actor, &channel).await?;
    let sender: Vec<u8> =
        query_scalar("SELECT sender_person FROM messages WHERE channel_id = ? AND id = ?")
            .bind(&channel)
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ApiError::not_found("message_not_found"))?;
    if sender != actor.person {
        roles::authority(&mut tx, &actor.person)
            .await?
            .require(Permission::DeleteMessages)?;
    }
    query("DELETE FROM messages WHERE channel_id = ? AND id = ?")
        .bind(&channel)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    let audit_id = audit(
        &mut tx,
        &actor.person,
        "message_deleted",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    if kind == "dm" {
        query("UPDATE audit_log SET dm_channel_id = ? WHERE id = ?")
            .bind(&channel)
            .bind(audit_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    app.notify(Some(Event::MessageDeleted { channel, id }));
    Ok(StatusCode::NO_CONTENT)
}

const _: () = assert!(BODY_LIMIT == 14_847);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_limit_holds_the_largest_message() {
        let json = format!(
            r#"{{"id":"{}","epoch":{},"envelope":"{}"}}"#,
            "A".repeat(43),
            i64::MAX,
            "A".repeat((MAX_MESSAGE_BYTES * 4).div_ceil(3)),
        );
        assert!(json.len() <= BODY_LIMIT);
        assert!(BODY_LIMIT - json.len() < 512);
        assert!(serde_json::from_str::<NewMessage>(&json).is_ok());
    }

    #[tokio::test]
    async fn retention_drops_the_oldest_record_in_one_channel() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = crate::store::open(dir.path()).await?;
        let mut tx = crate::store::write(&pool).await?;
        // Seed opaque rows directly so the test exercises the database cap without 10,001 signatures.
        sqlx::raw_sql(
            "INSERT INTO categories VALUES ('category', 0, X'00');
             INSERT INTO channels (id, kind, category_id, position, name_ct)
                 VALUES ('a', 'text', 'category', 0, X'00'), ('b', 'text', 'category', 0, X'00');
             INSERT INTO key_scopes (id, kind, channel_id)
                 SELECT id, 'channel', id FROM channels;
             INSERT INTO key_epochs SELECT id, 1, zeroblob(32) FROM key_scopes;
             INSERT INTO message_senders VALUES (X'01', X'02', X'03');",
        )
        .execute(&mut *tx)
        .await?;
        let limit = RETAIN + 1;
        query(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < ?)
             INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
                 SELECT CAST(i AS TEXT), 'a', 1, X'01', X'02', 0, 111, zeroblob(111) FROM n
                 UNION ALL SELECT 'other', 'b', 1, X'01', X'02', 0, 111, zeroblob(111)",
        )
        .bind(limit)
        .execute(&mut *tx)
        .await?;
        assert_eq!(prune(&mut tx, "a").await?, ["1"]);
        let counts: Vec<(String, i64)> = query_as(
            "SELECT channel_id, COUNT(*) FROM messages GROUP BY channel_id ORDER BY channel_id",
        )
        .fetch_all(&mut *tx)
        .await?;
        assert_eq!(counts, [("a".into(), RETAIN), ("b".into(), 1)]);
        assert!(prune(&mut tx, "a").await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn removed_ids_stay_reserved_and_dm_audit_survives_the_dm() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pool = crate::store::open(dir.path()).await?;
        let mut tx = crate::store::write(&pool).await?;
        reserve(&mut tx, "m").await?;
        assert_eq!(
            reserve(&mut tx, "m").await.unwrap_err().code,
            "message_exists"
        );
        sqlx::raw_sql(
            "INSERT INTO members (person_id, joined_at) VALUES (X'01', 0), (X'02', 0);
             INSERT INTO channels (id, kind, position, dm_first, dm_second)
                 VALUES ('dm', 'dm', 0, X'01', X'02');
             INSERT INTO audit_log (id, actor_person_id, action, target_id, at, dm_channel_id)
                 VALUES (1, X'01', 'message_deleted', 'm', 0, 'dm');
             DELETE FROM channels WHERE id = 'dm';",
        )
        .execute(&mut *tx)
        .await?;
        assert!(reserve(&mut tx, "m").await.is_err());
        let dm: Option<String> = query_scalar("SELECT dm_channel_id FROM audit_log WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
        assert_eq!(dm, None);
        Ok(())
    }
}
