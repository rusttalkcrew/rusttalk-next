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
    messages::{MAX_MESSAGE_BYTES, MessageContext, MessageRecord, Purpose},
};
use serde::Deserialize;
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate, authenticate_read},
    channels, history, keys, limits, page_files, payload,
    roles::{self, Permission},
    slow_mode,
    store::audit,
};

/// JSON for the largest envelope (unpadded base64url) plus 512 bytes for the ID, epoch and keys,
/// and 1,024 for a page's kept files.
pub const BODY_LIMIT: usize = (MAX_MESSAGE_BYTES * 4).div_ceil(3) + 1536;
const PAGE_LIMIT: i64 = 100;

#[derive(Deserialize)]
pub struct NewMessage {
    id: String,
    epoch: i64,
    envelope: String,
    backfill_at: Option<i64>,
    /// A page version's stored files, kept while it is the newest (see `page_files`).
    #[serde(default)]
    keep: Vec<String>,
    /// An edit, vote, reaction or a post's second part, which slow mode doesn't count.
    /// Apps send it only in slow mode channels; older ones never do.
    #[serde(default)]
    follow_up: bool,
}

#[derive(Deserialize)]
pub struct Page {
    before: Option<String>,
    limit: Option<i64>,
}

#[derive(FromRow)]
pub(crate) struct Row {
    backfilled: bool,
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
            backfilled: row.backfilled,
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
        backfill_at,
        keep,
        follow_up,
    } = payload(body)?;
    decode::<32>(&channel)?;
    let message_id = decode(&id)?;
    let envelope_bytes = decode_bytes(&envelope)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let action = match backfill_at {
        Some(_) => limits::Action::Backfill,
        None => limits::Action::Post,
    };
    app.limits.check(&actor, action)?;
    require_write(&mut tx, &actor, &channel).await?;
    let page = page_files::is_page(&mut tx, &channel).await?;
    let keep = match keep.is_empty() {
        true => keep,
        false => page_files::check(&mut tx, &channel, page, keep, actor.at).await?,
    };
    let at = match backfill_at {
        Some(at) if chat_kind(&mut tx, &actor, &channel).await? == "dm" => {
            historical_time(at, actor.at)
        }
        Some(_) => return Err(ApiError::bad_request("invalid_backfill")),
        None => actor.at,
    };
    let retain = history::keep(&mut tx).await?;
    // Historical inserts must not evict live history from a full replica.
    if backfill_at.is_some() && history_full(&mut tx, &channel, retain).await? {
        return Err(keys::conflict("history_full"));
    }
    let device = verify_signed(
        (Purpose::Message, message_id),
        &app,
        &mut tx,
        &actor,
        &channel,
        epoch,
        &envelope_bytes,
    )
    .await?;
    reserve(&mut tx, &id).await?;
    // Last, so a message refused for another reason doesn't start a wait.
    slow_mode::check(&mut tx, &actor, &channel, follow_up).await?;
    let size = envelope_bytes.len();
    query(
        "INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope, backfilled)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&channel)
    .bind(epoch)
    .bind(&actor.device)
    .bind(actor.person.as_slice())
    .bind(at)
    .bind(size as i64)
    .bind(envelope_bytes)
    .bind(backfill_at.is_some())
    .execute(&mut *tx)
    .await?;
    let dropped = prune(&mut tx, &channel, retain).await?;
    if page {
        page_files::record(&mut tx, &channel, &id, &keep).await?;
        page_files::refresh(&app, &mut tx, &channel).await?;
    }
    tx.commit().await?;
    for id in dropped {
        app.notify(Event::MessageDeleted {
            channel: channel.clone(),
            id,
        });
    }
    let record = MessageRecord {
        backfilled: backfill_at.is_some(),
        id: message_id,
        epoch,
        person: actor.person,
        sender_device: device,
        at,
        size,
        envelope,
    };
    app.notify(crate::events::message_created(channel, id, &record));
    Ok((StatusCode::CREATED, Json(record)))
}

/// Check a signed envelope made for `purpose` under `message_id` by this device in the channel's current
/// epoch, and keep its sender's certificate so readers can verify it after the device is gone.
pub(crate) async fn verify_signed(
    (purpose, message_id): (Purpose, [u8; 32]),
    app: &App,
    tx: &mut SqliteConnection,
    actor: &Actor,
    channel: &str,
    epoch: i64,
    envelope_bytes: &[u8],
) -> ApiResult<[u8; 32]> {
    let channel_id = decode(channel)?;
    let scope = Scope {
        kind: Kind::Channel,
        id: channel_id,
    };
    let (current, rotate) = keys::read_epoch(&mut *tx, actor, scope).await?;
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
        server_id: keys::space(app).id,
        channel_id,
        epoch,
        id: message_id,
        sender_device: device,
        person: actor.person,
    };
    context
        .verify_as(purpose, &verification_key, envelope_bytes)
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
    Ok(device)
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

fn historical_time(requested: i64, now: i64) -> i64 {
    requested.clamp(0, now)
}

/// Read the count `prune` and `uncount` keep, so a backfill burst never walks the channel's index.
/// A channel that keeps everything (`keep` 0) is never full.
async fn history_full(db: &mut SqliteConnection, channel: &str, keep: i64) -> ApiResult<bool> {
    if keep == 0 {
        return Ok(false);
    }
    let full: Option<bool> = query_scalar("SELECT message_count >= ? FROM channels WHERE id = ?")
        .bind(keep)
        .bind(channel)
        .fetch_optional(db)
        .await?;
    Ok(full.unwrap_or(false))
}

/// Count the message just inserted and drop the channel's oldest beyond `keep`.
async fn prune(db: &mut SqliteConnection, channel: &str, keep: i64) -> ApiResult<Vec<String>> {
    let count: i64 = query_scalar(
        "UPDATE channels SET message_count = message_count + 1 WHERE id = ? RETURNING message_count",
    )
    .bind(channel)
    .fetch_one(&mut *db)
    .await?;
    trim(db, channel, count, keep).await
}

/// Drop the oldest of a channel's `count` messages beyond `keep` (0 keeps all). The count lets
/// a full channel trim from its oldest end instead of walking past the rows it keeps.
pub(crate) async fn trim(
    db: &mut SqliteConnection,
    channel: &str,
    count: i64,
    keep: i64,
) -> ApiResult<Vec<String>> {
    if keep == 0 || count <= keep {
        return Ok(Vec::new());
    }
    let dropped: Vec<String> = query_scalar(
        "DELETE FROM messages WHERE channel_id = ? AND sequence <= (
             SELECT sequence FROM messages WHERE channel_id = ? ORDER BY sequence LIMIT 1 OFFSET ?
         ) RETURNING id",
    )
    .bind(channel)
    .bind(channel)
    .bind(count - keep - 1)
    .fetch_all(&mut *db)
    .await?;
    uncount(db, channel, dropped.len() as u64).await?;
    Ok(dropped)
}

async fn uncount(db: &mut SqliteConnection, channel: &str, removed: u64) -> ApiResult<()> {
    query("UPDATE channels SET message_count = message_count - ? WHERE id = ?")
        .bind(removed as i64)
        .bind(channel)
        .execute(db)
        .await?;
    Ok(())
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
        "SELECT m.backfilled, m.id, m.epoch, m.sender_person, m.sender_device, m.at, m.size, m.envelope
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
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
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
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    chat_kind(&mut tx, &actor, &channel).await?;
    decode::<32>(&id)?;
    let row: Row = query_as(
        "SELECT backfilled, id, epoch, sender_person, sender_device, at, size, envelope
         FROM messages WHERE channel_id = ? AND id = ?",
    )
    .bind(&channel)
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::not_found("message_not_found"))?;
    let record = MessageRecord::try_from(row)?;
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
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    chat_kind(&mut tx, &actor, &channel).await?;
    let certificate: Vec<u8> = query_scalar(
        "SELECT s.certificate FROM message_senders s
         WHERE s.device_id = ? AND s.person_id = ?
           AND (EXISTS (SELECT 1 FROM messages m WHERE m.channel_id = ?
             AND m.sender_device = s.device_id AND m.sender_person = s.person_id)
             OR EXISTS (SELECT 1 FROM shared_files h JOIN files f ON f.id = h.id
               WHERE f.channel_id = ?
               AND h.sender_device = s.device_id AND h.sender_person = s.person_id))",
    )
    .bind(device.as_slice())
    .bind(person.as_slice())
    .bind(&channel)
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
    app.limits.check(&actor, limits::Action::Delete)?;
    let kind = chat_kind(&mut tx, &actor, &channel).await?;
    let sender: Vec<u8> =
        query_scalar("SELECT sender_person FROM messages WHERE channel_id = ? AND id = ?")
            .bind(&channel)
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ApiError::not_found("message_not_found"))?;
    if sender != actor.person {
        // DMs are private to their two people: only the author deletes there.
        if kind == "dm" {
            return Err(ApiError::forbidden("permission_denied"));
        }
        roles::authority(&mut tx, &actor.person)
            .await?
            .require(Permission::DeleteMessages)?;
    }
    let removed = query("DELETE FROM messages WHERE channel_id = ? AND id = ?")
        .bind(&channel)
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    uncount(&mut tx, &channel, removed).await?;
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
    if page_files::is_page(&mut tx, &channel).await? {
        page_files::refresh(&app, &mut tx, &channel).await?;
    }
    tx.commit().await?;
    app.notify(Event::MessageDeleted { channel, id });
    Ok(StatusCode::NO_CONTENT)
}

const _: () = assert!(BODY_LIMIT == 20_104);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::DEFAULT as RETAIN;

    #[test]
    fn body_limit_holds_the_largest_message() {
        let keep = vec![format!("\"{}\"", "A".repeat(43)); page_files::MAX_KEPT].join(",");
        let json = format!(
            r#"{{"id":"{}","epoch":{},"envelope":"{}","backfill_at":null,"keep":[{keep}]}}"#,
            "A".repeat(43),
            i64::MAX,
            "A".repeat((MAX_MESSAGE_BYTES * 4).div_ceil(3)),
        );
        assert!(json.len() <= BODY_LIMIT);
        assert!(BODY_LIMIT - json.len() < 1024);
        assert!(serde_json::from_str::<NewMessage>(&json).is_ok());
    }

    #[test]
    fn historical_timestamp_stays_between_epoch_and_receipt() {
        assert_eq!(historical_time(i64::MIN, 10), 0);
        assert_eq!(historical_time(5, 10), 5);
        assert_eq!(historical_time(i64::MAX, 10), 10);
    }

    #[tokio::test]
    async fn backfill_endpoint_rejects_normal_channels_and_full_dms() -> anyhow::Result<()> {
        use crate::testing::{app, as_person, person};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let text = encode(&[3; 32]);
        let dm = encode(&[4; 32]);
        query("INSERT INTO categories VALUES ('category', 0, X'00')")
            .execute(&app.db)
            .await?;
        query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, 'text', 'category', 0, X'00')")
            .bind(&text).execute(&app.db).await?;
        query("INSERT INTO channel_permissions VALUES (?, 'owner', 3)")
            .bind(&text)
            .execute(&app.db)
            .await?;
        query("INSERT INTO channels (id, kind, position, dm_first, dm_second) VALUES (?, 'dm', 0, ?, ?)")
            .bind(&dm).bind(person(1)).bind(person(2)).execute(&app.db).await?;
        query("INSERT INTO key_scopes (id, kind, channel_id) VALUES (?, 'channel', ?)")
            .bind(&dm)
            .bind(&dm)
            .execute(&app.db)
            .await?;
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(&dm)
            .execute(&app.db)
            .await?;
        query("INSERT INTO message_senders VALUES (X'01', X'02', X'03')")
            .execute(&app.db)
            .await?;
        query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < ?)
             INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
             SELECT CAST(i AS TEXT), ?, 1, X'01', X'02', 0, 111, zeroblob(111) FROM n")
             .bind(RETAIN).bind(&dm).execute(&app.db).await?;
        query("UPDATE channels SET message_count = ? WHERE id = ?")
            .bind(RETAIN)
            .bind(&dm)
            .execute(&app.db)
            .await?;
        // Only a page version keeps files, and only from people who may write there.
        let kept = vec![encode(&[5; 32])];
        for (by, channel, backfill_at, keep, code) in [
            (1, &text, Some(1), vec![], "invalid_backfill"),
            (1, &dm, Some(1), vec![], "history_full"),
            (1, &text, None, kept.clone(), "invalid_keep"),
            (2, &text, None, kept, "channel_read_denied"),
        ] {
            let post = NewMessage {
                id: encode(&[9; 32]),
                epoch: 1,
                envelope: encode(&[]),
                backfill_at,
                keep,
                follow_up: false,
            };
            let result = create(
                State(app.clone()),
                as_person(by),
                Path(channel.clone()),
                Ok(Json(post)),
            )
            .await;
            assert_eq!(result.err().expect("must reject the post").code, code);
        }
        Ok(())
    }

    #[tokio::test]
    async fn backfill_stops_at_the_retention_limit_like_the_offset_query() -> anyhow::Result<()> {
        use crate::testing::{app, as_person, person};
        use ed25519_dalek::{Signer, SigningKey};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let dm = encode(&[4; 32]);
        let signer = SigningKey::from_bytes(&[8; 32]);
        let device = signer.verifying_key().to_bytes();
        query("UPDATE sessions SET device_id = ? WHERE person_id = ?")
            .bind(device.as_slice())
            .bind(person(1))
            .execute(&app.db)
            .await?;
        query("INSERT INTO member_devices VALUES (?, ?, X'00')")
            .bind(device.as_slice())
            .bind(person(1))
            .execute(&app.db)
            .await?;
        query("INSERT INTO channels (id, kind, position, dm_first, dm_second) VALUES (?, 'dm', 0, ?, ?)")
            .bind(&dm).bind(person(1)).bind(person(2)).execute(&app.db).await?;
        query("INSERT INTO key_scopes (id, kind, channel_id) VALUES (?, 'channel', ?)")
            .bind(&dm)
            .bind(&dm)
            .execute(&app.db)
            .await?;
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(&dm)
            .execute(&app.db)
            .await?;
        query("INSERT INTO message_senders VALUES (X'01', X'02', X'03')")
            .execute(&app.db)
            .await?;
        query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < ?)
             INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
             SELECT CAST(i AS TEXT), ?, 1, X'01', X'02', 0, 111, zeroblob(111) FROM n")
             .bind(RETAIN - 1).bind(&dm).execute(&app.db).await?;
        // What migration 0042 does for channels that already hold messages.
        query("UPDATE channels SET message_count = (SELECT COUNT(*) FROM messages WHERE channel_id = channels.id)")
            .execute(&app.db)
            .await?;

        // The smallest valid envelope, signed over the context `verify_signed` rebuilds.
        let backfill = |byte: u8| {
            let id = [byte; 32];
            let mut sealed = vec![3];
            sealed.extend([0; 24]);
            sealed.extend(20u16.to_be_bytes());
            sealed.extend([0; 20]);
            let signed = [
                b"rusttalk-message-v1".as_slice(),
                &keys::space(&app).id,
                &[4; 32],
                &1i64.to_be_bytes(),
                &id,
                &device,
                &[1; 32],
                &sealed,
            ]
            .concat();
            sealed.extend(signer.sign(&signed).to_bytes());
            let post = NewMessage {
                id: encode(&id),
                epoch: 1,
                envelope: encode(&sealed),
                backfill_at: Some(1),
                keep: vec![],
                follow_up: false,
            };
            create(
                State(app.clone()),
                as_person(1),
                Path(dm.clone()),
                Ok(Json(post)),
            )
        };
        let full = || async {
            let mut db = app.db.acquire().await?;
            let old: bool = query_scalar(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE channel_id = ? LIMIT 1 OFFSET ?)",
            )
            .bind(&dm)
            .bind(RETAIN - 1)
            .fetch_one(&mut *db)
            .await?;
            assert_eq!(history_full(&mut db, &dm, RETAIN).await?, old);
            anyhow::Ok(old)
        };

        assert!(!full().await?);
        let (status, Json(record)) = backfill(9).await?;
        assert_eq!(status, StatusCode::CREATED);
        assert!(record.backfilled);
        assert!(full().await?);
        let refused = backfill(10)
            .await
            .err()
            .expect("a full DM refuses backfill");
        assert_eq!(
            (refused.status, refused.code),
            (StatusCode::CONFLICT, "history_full")
        );
        Ok(())
    }

    /// Two readable text channels with valid IDs, so handlers accept them.
    const A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const B: &str = "EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE";

    async fn two_channels(db: &mut SqliteConnection) -> anyhow::Result<()> {
        query("INSERT INTO categories VALUES ('category', 0, X'00')")
            .execute(&mut *db)
            .await?;
        query(
            "INSERT INTO channels (id, kind, category_id, position, name_ct)
                 VALUES (?, 'text', 'category', 0, X'00'), (?, 'text', 'category', 0, X'00')",
        )
        .bind(A)
        .bind(B)
        .execute(&mut *db)
        .await?;
        sqlx::raw_sql(
            "INSERT INTO channel_permissions SELECT id, 'owner', 3 FROM channels;
             INSERT INTO key_scopes (id, kind, channel_id) SELECT id, 'channel', id FROM channels;
             INSERT INTO key_epochs (scope_id, epoch, commitment) SELECT id, 1, zeroblob(32) FROM key_scopes;",
        )
        .execute(db)
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn retention_drops_the_oldest_record_in_one_channel() -> anyhow::Result<()> {
        use crate::testing::{app, as_person};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 1).await?;
        let mut tx = crate::store::write(&app.db).await?;
        two_channels(&mut tx).await?;
        // Seed opaque rows directly so the test exercises the database cap without 10,001 signatures.
        query("INSERT INTO message_senders VALUES (X'01', X'02', X'03')")
            .execute(&mut *tx)
            .await?;
        query(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < ?)
             INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
                 SELECT CAST(i AS TEXT), ?, 1, X'01', X'02', 0, 111, zeroblob(111) FROM n
                 UNION ALL SELECT 'other', ?, 1, X'01', X'02', 0, 111, zeroblob(111)",
        )
        .bind(RETAIN - 1)
        .bind(A)
        .bind(B)
        .execute(&mut *tx)
        .await?;
        // What migration 0042 does for channels that already hold messages.
        query("UPDATE channels SET message_count = (SELECT COUNT(*) FROM messages WHERE channel_id = channels.id)")
            .execute(&mut *tx)
            .await?;

        // Post as `create` does, and check the trim against the previous whole-channel query.
        async fn post(tx: &mut SqliteConnection, id: &str) -> anyhow::Result<Vec<String>> {
            query(
                "INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
                 VALUES (?, ?, 1, X'01', X'02', 0, 111, zeroblob(111))",
            )
            .bind(id)
            .bind(A)
            .execute(&mut *tx)
            .await?;
            let before: Vec<String> = query_scalar(
                "SELECT id FROM messages WHERE channel_id = ? AND sequence <= (
                     SELECT sequence FROM messages WHERE channel_id = ? ORDER BY sequence DESC LIMIT 1 OFFSET ?
                 ) ORDER BY sequence",
            )
            .bind(A)
            .bind(A)
            .bind(RETAIN)
            .fetch_all(&mut *tx)
            .await?;
            let dropped = prune(tx, A, RETAIN).await?;
            assert_eq!(dropped, before);
            Ok(dropped)
        }
        async fn kept(tx: &mut SqliteConnection) -> anyhow::Result<Vec<(String, i64, i64)>> {
            Ok(query_as(
                "SELECT c.id, c.message_count, (SELECT COUNT(*) FROM messages m WHERE m.channel_id = c.id)
                 FROM channels c ORDER BY c.id",
            )
            .fetch_all(tx)
            .await?)
        }

        assert!(!history_full(&mut tx, A, RETAIN).await?);
        let newest = encode(&[1; 32]);
        assert!(post(&mut tx, &newest).await?.is_empty());
        assert!(history_full(&mut tx, A, RETAIN).await?);
        assert!(!history_full(&mut tx, B, RETAIN).await?);
        assert_eq!(post(&mut tx, "10001").await?, ["1"]);
        assert_eq!(post(&mut tx, "10002").await?, ["2"]);
        assert_eq!(
            kept(&mut tx).await?,
            [(A.into(), RETAIN, RETAIN), (B.into(), 1, 1)]
        );
        tx.commit().await?;

        // A deleted message frees one place, so the next post keeps everything.
        let status = delete(State(app.clone()), as_person(1), Path((A.into(), newest))).await?;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let mut tx = crate::store::write(&app.db).await?;
        assert_eq!(kept(&mut tx).await?[0], (A.into(), RETAIN - 1, RETAIN - 1));
        assert!(post(&mut tx, "10003").await?.is_empty());
        assert_eq!(post(&mut tx, "10004").await?, ["3"]);
        assert_eq!(kept(&mut tx).await?[0], (A.into(), RETAIN, RETAIN));
        Ok(())
    }

    #[tokio::test]
    async fn get_returns_what_the_two_query_lookup_returned() -> anyhow::Result<()> {
        use crate::testing::{app, as_person, person};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 1).await?;
        two_channels(&mut *app.db.acquire().await?).await?;
        query("INSERT INTO message_senders VALUES (?, ?, X'03')")
            .bind([2; 32].as_slice())
            .bind(person(1))
            .execute(&app.db)
            .await?;
        let ids: Vec<String> = (1..=3).map(|n| encode(&[n; 32])).collect();
        let channels = [A, A, B];
        for (n, (id, channel)) in ids.iter().zip(channels).enumerate() {
            query(
                "INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope, backfilled)
                 VALUES (?, ?, 1, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(channel)
            .bind([2; 32].as_slice())
            .bind(person(1))
            .bind(n as i64 + 5)
            .bind(111 + n as i64)
            .bind(vec![n as u8; 111 + n])
            .bind(n == 1)
            .execute(&app.db)
            .await?;
        }
        let fetch = |channel: &str, id: &str| {
            get(
                State(app.clone()),
                as_person(1),
                Path((channel.to_string(), id.to_string())),
            )
        };
        for (id, channel) in ids.iter().zip(channels) {
            let Json(record) = fetch(channel, id).await?;
            let mut db = app.db.acquire().await?;
            let through = sequence(&mut db, channel, id).await?;
            let old = records(&mut db, channel, through, 1).await?;
            assert_eq!(
                serde_json::to_value(&record)?,
                serde_json::to_value(&old[0])?
            );
        }
        // Another channel's message, an unknown one and a malformed ID fail as before.
        let unknown = encode(&[9; 32]);
        for (id, code) in [
            (&ids[2], "message_not_found"),
            (&unknown, "message_not_found"),
            (&"short".to_string(), "invalid_encoding"),
        ] {
            let code_now = fetch(A, id).await.err().map(|e| e.code);
            let mut db = app.db.acquire().await?;
            let code_before = sequence(&mut db, A, id).await.err().map(|e| e.code);
            assert_eq!(code_now, code_before);
            assert_eq!(code_now, Some(code));
        }
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
