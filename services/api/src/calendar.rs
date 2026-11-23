//! Server events. Their content is sealed and signed on devices; the server checks who
//! may add, approve and delete them, and deletes them when due.

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use ed25519_dalek::VerifyingKey;
use rusttalk_core::{
    auth::{decode, decode_bytes, encode},
    calendar::{
        AnswerContext, AnswerRecord, Calendar, CalendarSetting, EventContext, EventRecord, GRACE,
        HORIZON, NewAnswer, NewEvent, REMIND_EVERY,
    },
    keys::Event,
};
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    keys, payload,
    roles::{self, Permission},
    store::{self, audit},
};

#[cfg(test)]
mod tests;

/// Suggestions one person can have waiting.
const MAX_WAITING: i64 = 5;
/// Events and suggestions one server holds.
const MAX_EVENTS: i64 = 50;

#[derive(FromRow)]
struct Row {
    id: String,
    author: Vec<u8>,
    author_device: Vec<u8>,
    certificate: Vec<u8>,
    approved: bool,
    epoch: i64,
    remove_at: i64,
    envelope: Vec<u8>,
    reminded_at: Option<i64>,
}

#[derive(FromRow)]
struct AnswerRow {
    person: Vec<u8>,
    device: Vec<u8>,
    certificate: Vec<u8>,
    epoch: i64,
    envelope: Vec<u8>,
}

impl Row {
    fn record(self, answers: Vec<AnswerRow>) -> ApiResult<EventRecord> {
        let invalid = || ApiError::internal("invalid_stored_event");
        Ok(EventRecord {
            id: decode(&self.id).map_err(|_| invalid())?,
            author: self.author.try_into().map_err(|_| invalid())?,
            author_device: self.author_device.try_into().map_err(|_| invalid())?,
            certificate: encode(&self.certificate),
            approved: self.approved,
            epoch: self.epoch,
            remove_at: self.remove_at,
            envelope: encode(&self.envelope),
            answers: answers
                .into_iter()
                .map(|row| {
                    Ok(AnswerRecord {
                        person: row.person.try_into().map_err(|_| invalid())?,
                        device: row.device.try_into().map_err(|_| invalid())?,
                        certificate: encode(&row.certificate),
                        epoch: row.epoch,
                        envelope: encode(&row.envelope),
                    })
                })
                .collect::<ApiResult<_>>()?,
            reminded_at: self.reminded_at,
        })
    }
}

async fn enabled(db: &mut SqliteConnection) -> ApiResult<bool> {
    Ok(
        query_scalar("SELECT events_enabled FROM server_settings WHERE id = 1")
            .fetch_one(db)
            .await?,
    )
}

async fn require_enabled(db: &mut SqliteConnection) -> ApiResult<()> {
    if !enabled(db).await? {
        return Err(keys::conflict("events_disabled"));
    }
    Ok(())
}

pub async fn list(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Calendar>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    let enabled = enabled(&mut tx).await?;
    let manages = roles::authority(&mut tx, &actor.person)
        .await?
        .has(Permission::ManageEvents);
    let rows: Vec<Row> = query_as(
        "SELECT id, author, author_device, certificate, approved, epoch, remove_at, envelope,
                reminded_at
         FROM calendar_events
         WHERE ? AND remove_at > ? AND (approved OR ? OR author = ?)
         ORDER BY remove_at, id",
    )
    .bind(enabled)
    .bind(actor.at)
    .bind(manages)
    .bind(actor.person.as_slice())
    .fetch_all(&mut *tx)
    .await?;
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let answers = query_as(
            "SELECT person, device, certificate, epoch, envelope FROM calendar_answers
             WHERE event_id = ? ORDER BY person",
        )
        .bind(&row.id)
        .fetch_all(&mut *tx)
        .await?;
        events.push(row.record(answers)?);
    }
    tx.commit().await?;
    Ok(Json(Calendar { enabled, events }))
}

pub async fn set_enabled(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<CalendarSetting>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let on = payload(body)?.enabled;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageServer)?;
    let changed =
        query("UPDATE server_settings SET events_enabled = ? WHERE id = 1 AND events_enabled <> ?")
            .bind(on)
            .bind(on)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            > 0;
    if changed {
        let action = if on {
            "events_enabled"
        } else {
            "events_disabled"
        };
        audit(&mut tx, &actor.person, action, None, None, actor.at).await?;
    }
    tx.commit().await?;
    if changed {
        app.notify(Event::ChannelChanged);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewEvent>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<EventRecord>)> {
    let new = payload(body)?;
    let envelope = decode_bytes(&new.envelope)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let authority = roles::authority(&mut tx, &actor.person).await?;
    let approved = authority.has(Permission::CreateEvents);
    if !approved {
        authority.require(Permission::SuggestEvents)?;
    }
    require_enabled(&mut tx).await?;
    let ends_after = actor.at + GRACE;
    if new.remove_at <= ends_after || new.remove_at > ends_after + HORIZON {
        return Err(ApiError::bad_request("invalid_event_time"));
    }
    let space = keys::space(&app);
    let (current, rotate) = keys::read_epoch(&mut tx, &actor, space).await?;
    if rotate || new.epoch != current {
        return Err(keys::conflict("epoch_changed"));
    }

    let invalid_device = || ApiError::internal("invalid_stored_device");
    let device: [u8; 32] = actor
        .device
        .as_slice()
        .try_into()
        .map_err(|_| invalid_device())?;
    let context = EventContext {
        server_id: space.id,
        id: new.id,
        epoch: new.epoch,
        remove_at: new.remove_at,
        author_device: device,
        author: actor.person,
    };
    let signer = VerifyingKey::from_bytes(&device).map_err(|_| invalid_device())?;
    context
        .verify(&signer, &envelope)
        .map_err(|_| ApiError::bad_request("invalid_event"))?;
    let certificate: Vec<u8> = query_scalar(
        "SELECT certificate FROM member_devices WHERE device_id = ? AND person_id = ?",
    )
    .bind(&actor.device)
    .bind(actor.person.as_slice())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::forbidden("device_certificate_required"))?;

    let (stored, waiting): (i64, i64) = query_as(
        "SELECT COUNT(*), COUNT(CASE WHEN NOT approved AND author = ? THEN 1 END)
         FROM calendar_events WHERE remove_at > ?",
    )
    .bind(actor.person.as_slice())
    .bind(actor.at)
    .fetch_one(&mut *tx)
    .await?;
    if !approved && waiting >= MAX_WAITING {
        return Err(keys::conflict("suggestion_limit"));
    }
    if stored >= MAX_EVENTS {
        return Err(keys::conflict("event_limit"));
    }

    // Event and channel IDs share one namespace and are never used twice.
    let id = encode(&new.id);
    let reserved = query("INSERT INTO channel_ids (id) VALUES (?) ON CONFLICT (id) DO NOTHING")
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if reserved == 0 || new.id == space.id {
        return Err(keys::conflict("event_exists"));
    }
    query(
        "INSERT INTO calendar_events
             (id, author, author_device, certificate, approved, epoch, remove_at, envelope)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(actor.person.as_slice())
    .bind(&actor.device)
    .bind(&certificate)
    .bind(approved)
    .bind(new.epoch)
    .bind(new.remove_at)
    .bind(&envelope)
    .execute(&mut *tx)
    .await?;
    if approved {
        audit(
            &mut tx,
            &actor.person,
            "event_created",
            None,
            Some(&id),
            actor.at,
        )
        .await?;
    }
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok((
        StatusCode::CREATED,
        Json(EventRecord {
            id: new.id,
            author: actor.person,
            author_device: device,
            certificate: encode(&certificate),
            approved,
            epoch: new.epoch,
            remove_at: new.remove_at,
            envelope: new.envelope,
            answers: Vec::new(),
            reminded_at: None,
        }),
    ))
}

pub async fn approve(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageEvents)?;
    require_enabled(&mut tx).await?;
    let (author, approved, remove_at): (Vec<u8>, bool, i64) = query_as(
        "SELECT author, approved, remove_at FROM calendar_events WHERE id = ? AND remove_at > ?",
    )
    .bind(&id)
    .bind(actor.at)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::not_found("event_not_found"))?;
    if approved {
        return Err(keys::conflict("event_approved"));
    }
    if remove_at <= actor.at + GRACE {
        return Err(keys::conflict("event_ended"));
    }
    query("UPDATE calendar_events SET approved = TRUE WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &actor.person,
        "event_approved",
        Some(&author),
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}

/// Withdrawing (the author), declining or cancelling (event managers). Works while Events
/// is off too, so nothing is stuck.
pub async fn remove(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let manages = roles::authority(&mut tx, &actor.person)
        .await?
        .has(Permission::ManageEvents);
    // Someone else's suggestion answers like a missing one, so it stays unknown to others.
    let (author, approved): (Vec<u8>, bool) = query_as(
        "SELECT author, approved FROM calendar_events
         WHERE id = ? AND (approved OR ? OR author = ?)",
    )
    .bind(&id)
    .bind(manages)
    .bind(actor.person.as_slice())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::not_found("event_not_found"))?;
    let own = author == actor.person;
    if !own && !manages {
        return Err(ApiError::forbidden("permission_denied"));
    }
    query("DELETE FROM calendar_events WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    let action = match (approved, own) {
        (true, _) => Some("event_cancelled"),
        (false, false) => Some("event_declined"),
        (false, true) => None,
    };
    if let Some(action) = action {
        let target = (!own).then_some(author.as_slice());
        audit(&mut tx, &actor.person, action, target, Some(&id), actor.at).await?;
    }
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}

/// Delete events that are due. The audit log needs an actor, so this only logs.
pub async fn expire(app: &App) -> ApiResult<()> {
    let mut tx = store::write(&app.db).await?;
    let gone: Vec<String> =
        query_scalar("DELETE FROM calendar_events WHERE remove_at <= ? RETURNING id")
            .bind(store::now()?)
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    for id in &gone {
        tracing::info!(id, "event expired");
    }
    if !gone.is_empty() {
        app.notify(Event::ChannelChanged);
    }
    Ok(())
}

/// The event `id` if it is approved and not over yet; answering and reminding need both.
async fn upcoming(db: &mut SqliteConnection, id: &str, at: i64) -> ApiResult<Vec<u8>> {
    let (author, approved, remove_at): (Vec<u8>, bool, i64) = query_as(
        "SELECT author, approved, remove_at FROM calendar_events WHERE id = ? AND remove_at > ?",
    )
    .bind(id)
    .bind(at)
    .fetch_optional(&mut *db)
    .await?
    .ok_or(ApiError::not_found("event_not_found"))?;
    if !approved {
        return Err(ApiError::not_found("event_not_found"));
    }
    if remove_at <= at + GRACE {
        return Err(keys::conflict("event_ended"));
    }
    Ok(author)
}

/// Going or not going, sealed so the server only learns that someone answered.
pub async fn answer(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NewAnswer>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let event_id = decode(&id)?;
    let new = payload(body)?;
    let envelope = decode_bytes(&new.envelope)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    require_enabled(&mut tx).await?;
    upcoming(&mut tx, &id, actor.at).await?;
    let space = keys::space(&app);
    let (current, rotate) = keys::read_epoch(&mut tx, &actor, space).await?;
    if rotate || new.epoch != current {
        return Err(keys::conflict("epoch_changed"));
    }
    let invalid_device = || ApiError::internal("invalid_stored_device");
    let device: [u8; 32] = actor
        .device
        .as_slice()
        .try_into()
        .map_err(|_| invalid_device())?;
    let context = AnswerContext {
        server_id: space.id,
        event_id,
        epoch: new.epoch,
        device,
        person: actor.person,
    };
    let signer = VerifyingKey::from_bytes(&device).map_err(|_| invalid_device())?;
    context
        .verify(&signer, &envelope)
        .map_err(|_| ApiError::bad_request("invalid_answer"))?;
    let certificate: Vec<u8> = query_scalar(
        "SELECT certificate FROM member_devices WHERE device_id = ? AND person_id = ?",
    )
    .bind(&actor.device)
    .bind(actor.person.as_slice())
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::forbidden("device_certificate_required"))?;
    query(
        "INSERT INTO calendar_answers (event_id, person, device, certificate, epoch, envelope)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (event_id, person) DO UPDATE SET device = excluded.device,
             certificate = excluded.certificate, epoch = excluded.epoch,
             envelope = excluded.envelope",
    )
    .bind(&id)
    .bind(actor.person.as_slice())
    .bind(&actor.device)
    .bind(&certificate)
    .bind(new.epoch)
    .bind(&envelope)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}

/// Remind those who haven't answered: the author or an event manager, once an hour.
pub async fn remind(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    decode::<32>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let manages = roles::authority(&mut tx, &actor.person)
        .await?
        .has(Permission::ManageEvents);
    require_enabled(&mut tx).await?;
    let author = upcoming(&mut tx, &id, actor.at).await?;
    if !manages && author != actor.person {
        return Err(ApiError::forbidden("permission_denied"));
    }
    let reminded = query(
        "UPDATE calendar_events SET reminded_at = ?
         WHERE id = ? AND (reminded_at IS NULL OR reminded_at <= ?)",
    )
    .bind(actor.at)
    .bind(&id)
    .bind(actor.at - REMIND_EVERY)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if reminded == 0 {
        return Err(keys::conflict("remind_too_soon"));
    }
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}
