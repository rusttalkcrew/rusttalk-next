use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, decode_bytes, encode},
    identity::DeviceCertificate,
    keys::{
        EnvelopeContext, Event, KeyNeed, KeyState, Kind, MemberDevice, Scope, ScopeState,
        WrappedKey,
    },
};
use serde::Deserialize;
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate},
    payload,
};

pub fn space(app: &App) -> Scope {
    Scope {
        kind: Kind::Space,
        id: app.key.verifying_key().to_bytes(),
    }
}

pub async fn devices(
    State(app): State<App>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<MemberDevice>>> {
    let (mut tx, _) = authenticate(&app, &headers).await?;
    let rows: Vec<(Vec<u8>, Vec<u8>)> =
        query_as("SELECT person_id, certificate FROM member_devices ORDER BY person_id, device_id")
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Json(
        rows.into_iter()
            .map(|(person, certificate)| MemberDevice {
                person: encode(&person),
                certificate: encode(&certificate),
            })
            .collect(),
    ))
}

pub async fn register_device(
    db: &mut SqliteConnection,
    cert: &DeviceCertificate,
) -> ApiResult<bool> {
    let device = cert.signing_key().as_bytes().as_slice();
    let certificate = cert
        .public_bytes()
        .map_err(|_| ApiError::bad_request("invalid_certificate"))?;
    let registered: Option<(Vec<u8>, Vec<u8>)> =
        query_as("SELECT person_id, certificate FROM member_devices WHERE device_id = ?")
            .bind(device)
            .fetch_optional(&mut *db)
            .await?;
    if let Some((person, stored)) = registered {
        if person.as_slice() != cert.person_id() || stored.as_slice() != certificate {
            return Err(conflict("device_certificate_changed"));
        }
        return Ok(false);
    }
    let devices: i64 = query_scalar("SELECT COUNT(*) FROM member_devices WHERE person_id = ?")
        .bind(cert.person_id().as_slice())
        .fetch_one(&mut *db)
        .await?;
    if devices >= 10 {
        return Err(conflict("device_limit"));
    }
    query("INSERT INTO member_devices (device_id, person_id, certificate) VALUES (?, ?, ?)")
        .bind(device)
        .bind(cert.person_id().as_slice())
        .bind(certificate)
        .execute(db)
        .await?;
    Ok(true)
}

pub async fn readable_channels(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<Vec<String>> {
    Ok(
        query_scalar("SELECT channel_id FROM channel_readers WHERE person_id = ?")
            .bind(person)
            .fetch_all(db)
            .await?,
    )
}

pub async fn lost_channels(
    db: &mut SqliteConnection,
    person: &[u8],
    before: Vec<String>,
) -> ApiResult<()> {
    let after = readable_channels(db, person).await?;
    for id in before {
        if !after.contains(&id) {
            mark(db, &id).await?;
        }
    }
    Ok(())
}

pub async fn member_removed(app: &App, db: &mut SqliteConnection, person: &[u8]) -> ApiResult<()> {
    let channels = readable_channels(db, person).await?;
    mark(db, &encode(&space(app).id)).await?;
    for id in channels {
        mark(db, &id).await?;
    }
    Ok(())
}

pub async fn mark(db: &mut SqliteConnection, id: &str) -> ApiResult<()> {
    query(
        "UPDATE key_scopes SET rotate = TRUE WHERE id = ?
           AND EXISTS (SELECT 1 FROM key_epochs WHERE scope_id = ?)",
    )
    .bind(id)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

#[derive(FromRow)]
struct EpochRow {
    id: String,
    kind: String,
    epoch: i64,
    rotate: bool,
}

fn scope(id: &str, kind: &str) -> ApiResult<Scope> {
    Ok(Scope {
        id: decode(id)?,
        kind: if kind == "space" {
            Kind::Space
        } else {
            Kind::Channel
        },
    })
}

pub async fn scopes(db: &mut SqliteConnection, actor: &Actor) -> ApiResult<Vec<ScopeState>> {
    let rows: Vec<EpochRow> = query_as(
        "SELECT k.id, k.kind, COALESCE(MAX(e.epoch), 0) AS epoch, k.rotate
         FROM scope_readers r JOIN key_scopes k ON r.scope_id = k.id
         LEFT JOIN key_epochs e ON k.id = e.scope_id
         WHERE r.person_id = ? GROUP BY k.id, k.kind, k.rotate ORDER BY k.id",
    )
    .bind(actor.person.as_slice())
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(ScopeState {
                scope: scope(&row.id, &row.kind)?,
                epoch: row.epoch,
                rotate: row.rotate,
            })
        })
        .collect()
}

#[derive(FromRow)]
struct NeedRow {
    scope_id: String,
    kind: String,
    epoch: i64,
    device_id: Vec<u8>,
}

pub async fn pending(db: &mut SqliteConnection, actor: &Actor) -> ApiResult<Vec<KeyNeed>> {
    let rows: Vec<NeedRow> = query_as(
        "SELECT e.scope_id, k.kind, e.epoch, d.device_id
         FROM scope_readers a JOIN key_scopes k ON a.scope_id = k.id
         JOIN key_epochs e ON a.scope_id = e.scope_id
         JOIN scope_readers r ON a.scope_id = r.scope_id
         JOIN member_devices d ON r.person_id = d.person_id
         WHERE a.person_id = ?
           AND NOT EXISTS (SELECT 1 FROM key_envelope_rejections x
               WHERE x.scope_id = e.scope_id AND x.epoch = e.epoch
                 AND x.recipient_device = d.device_id AND x.sender_person = a.person_id)
           AND NOT EXISTS (SELECT 1 FROM key_envelopes v
               JOIN member_devices s ON s.device_id = v.sender_device
               WHERE v.scope_id = e.scope_id AND v.epoch = e.epoch AND v.recipient_device = d.device_id)
         ORDER BY e.scope_id, e.epoch, d.device_id",
    ).bind(actor.person.as_slice()).fetch_all(db).await?;
    rows.into_iter()
        .map(|row| {
            Ok(KeyNeed {
                scope: scope(&row.scope_id, &row.kind)?,
                epoch: row.epoch,
                device: encode(&row.device_id),
            })
        })
        .collect()
}

#[derive(FromRow)]
struct EnvelopeRow {
    id: String,
    kind: String,
    epoch: i64,
    sender_device: Vec<u8>,
    commitment: Vec<u8>,
    envelope: Vec<u8>,
}

fn stored_commitment(bytes: Vec<u8>) -> ApiResult<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| ApiError::internal("invalid_stored_commitment"))
}

pub async fn state(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<KeyState>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let rows: Vec<EnvelopeRow> = query_as(
        "SELECT k.id, k.kind, e.epoch, e.sender_device, v.commitment, e.envelope FROM key_envelopes e
         JOIN key_epochs v ON v.scope_id = e.scope_id AND v.epoch = e.epoch
         JOIN scope_readers r ON r.scope_id = e.scope_id JOIN key_scopes k ON k.id = e.scope_id
         JOIN member_devices s ON s.device_id = e.sender_device
         WHERE r.person_id = ? AND e.recipient_device = ? ORDER BY k.id, e.epoch",
    )
    .bind(actor.person.as_slice())
    .bind(&actor.device)
    .fetch_all(&mut *tx)
    .await?;
    let envelopes = rows
        .into_iter()
        .map(|row| {
            Ok(WrappedKey {
                need: KeyNeed {
                    scope: scope(&row.id, &row.kind)?,
                    epoch: row.epoch,
                    device: encode(&actor.device),
                },
                sender: encode(&row.sender_device),
                commitment: stored_commitment(row.commitment)?,
                envelope: encode(&row.envelope),
            })
        })
        .collect::<ApiResult<_>>()?;
    let state = KeyState {
        scopes: scopes(&mut tx, &actor).await?,
        envelopes,
        pending: pending(&mut tx, &actor).await?,
    };
    tx.commit().await?;
    Ok(Json(state))
}

#[derive(Deserialize)]
pub struct Upload {
    #[serde(flatten)]
    need: KeyNeed,
    envelope: String,
}

pub fn conflict(code: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code)
}

pub async fn space_epoch(db: &mut SqliteConnection, app: &App) -> ApiResult<i64> {
    let (epoch, rotate): (i64, bool) = query_as(
        "SELECT COALESCE(MAX(e.epoch), 0), COALESCE(k.rotate, FALSE)
         FROM key_scopes k LEFT JOIN key_epochs e ON k.id = e.scope_id WHERE k.id = ?
         GROUP BY k.rotate",
    )
    .bind(encode(&space(app).id))
    .fetch_optional(db)
    .await?
    .unwrap_or_default();
    if epoch == 0 || rotate {
        Err(conflict("space_key_unavailable"))
    } else {
        Ok(epoch)
    }
}

pub async fn init_space(db: &mut SqliteConnection, id: &[u8; 32]) -> ApiResult<()> {
    query("INSERT INTO key_scopes (id, kind) VALUES (?, 'space') ON CONFLICT (id) DO NOTHING")
        .bind(encode(id))
        .execute(db)
        .await?;
    Ok(())
}

async fn read_epoch(
    db: &mut SqliteConnection,
    actor: &Actor,
    scope: Scope,
) -> ApiResult<(i64, bool)> {
    let id = encode(&scope.id);
    let kind = match scope.kind {
        Kind::Space => "space",
        Kind::Channel => "channel",
    };
    query_as(
        "SELECT COALESCE(MAX(e.epoch), 0), k.rotate
         FROM scope_readers r JOIN key_scopes k ON r.scope_id = k.id
         LEFT JOIN key_epochs e ON k.id = e.scope_id
         WHERE k.id = ? AND k.kind = ? AND r.person_id = ? GROUP BY k.rotate",
    )
    .bind(&id)
    .bind(kind)
    .bind(actor.person.as_slice())
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::forbidden("scope_read_denied"))
}

async fn checked_envelope(
    db: &mut SqliteConnection,
    actor: &Actor,
    context: EnvelopeContext,
    device: &[u8; 32],
    envelope: &str,
) -> ApiResult<Vec<u8>> {
    let stored: Vec<u8> = query_scalar(
        "SELECT certificate FROM member_devices
            WHERE device_id = ? AND person_id = ?",
    )
    .bind(&actor.device)
    .bind(actor.person.as_slice())
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::forbidden("device_certificate_required"))?;
    let sender = DeviceCertificate::verify_public(actor.person, &stored)
        .map_err(|_| ApiError::internal("invalid_stored_certificate"))?;
    let bytes = decode_bytes(envelope)?;
    context
        .verify(device, &sender, &bytes)
        .map_err(|_| ApiError::bad_request("invalid_envelope"))?;
    Ok(bytes)
}

pub async fn start_epoch(
    db: &mut SqliteConnection,
    actor: &Actor,
    context: EnvelopeContext,
    envelope: &str,
) -> ApiResult<()> {
    let EnvelopeContext {
        scope,
        epoch,
        commitment,
        ..
    } = context;
    let (current, rotate) = read_epoch(db, actor, scope).await?;
    if current.checked_add(1) != Some(epoch) || (current != 0 && !rotate) {
        return Err(conflict("epoch_changed"));
    }
    if scope.kind == Kind::Space && current == 0 {
        let is_owner: bool =
            query_scalar("SELECT owner_person_id = ? FROM server_settings WHERE id = 1")
                .bind(actor.person.as_slice())
                .fetch_one(&mut *db)
                .await?;
        if !is_owner {
            return Err(ApiError::forbidden("owner_required"));
        }
    }
    let device = actor
        .device
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::internal("invalid_stored_device"))?;
    let bytes = checked_envelope(db, actor, context, device, envelope).await?;
    let id = encode(&scope.id);
    query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(epoch)
        .bind(commitment.as_slice())
        .execute(&mut *db)
        .await?;
    query(
        "INSERT INTO key_envelopes (scope_id, epoch, recipient_device, sender_device, envelope)
           VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(epoch)
    .bind(&actor.device)
    .bind(&actor.device)
    .bind(bytes)
    .execute(&mut *db)
    .await?;
    query("UPDATE key_scopes SET rotate = FALSE WHERE id = ?")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct NewEpoch {
    scope: Scope,
    epoch: i64,
    commitment: String,
    envelope: String,
}

pub async fn new_epoch(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewEpoch>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let NewEpoch {
        scope,
        epoch,
        commitment,
        envelope,
    } = payload(body)?;
    let context = EnvelopeContext {
        server_id: space(&app).id,
        scope,
        epoch,
        commitment: decode(&commitment)?,
    };
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    start_epoch(&mut tx, &actor, context, &envelope).await?;
    tx.commit().await?;
    app.notify(None);
    Ok(StatusCode::CREATED)
}

pub async fn upload(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Upload>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Upload {
        need: KeyNeed {
            scope,
            epoch,
            device,
        },
        envelope,
    } = payload(body)?;
    let device = decode::<32>(&device)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let (current, _) = read_epoch(&mut tx, &actor, scope).await?;
    if !(1..=current).contains(&epoch) {
        return Err(conflict("epoch_changed"));
    }
    let id = encode(&scope.id);
    let eligible: bool = query_scalar("SELECT EXISTS (SELECT 1 FROM member_devices d
        JOIN scope_readers r ON d.person_id = r.person_id WHERE d.device_id = ? AND r.scope_id = ?)")
        .bind(device.as_slice()).bind(&id).fetch_one(&mut *tx).await?;
    if !eligible {
        return Err(ApiError::forbidden("recipient_read_denied"));
    }
    let commitment =
        query_scalar("SELECT commitment FROM key_epochs WHERE scope_id = ? AND epoch = ?")
            .bind(&id)
            .bind(epoch)
            .fetch_one(&mut *tx)
            .await?;
    let context = EnvelopeContext {
        server_id: space(&app).id,
        scope,
        epoch,
        commitment: stored_commitment(commitment)?,
    };
    let bytes = checked_envelope(&mut tx, &actor, context, &device, &envelope).await?;
    let inserted = query("INSERT INTO key_envelopes (scope_id, epoch, recipient_device, sender_device, envelope)
        SELECT ?, ?, ?, ?, ? WHERE NOT EXISTS (
            SELECT 1 FROM key_envelope_rejections
            WHERE scope_id = ? AND epoch = ? AND recipient_device = ? AND sender_person = ?
        ) ON CONFLICT (scope_id, epoch, recipient_device) DO UPDATE
        SET sender_device = excluded.sender_device, envelope = excluded.envelope
        WHERE NOT EXISTS (SELECT 1 FROM member_devices WHERE device_id = key_envelopes.sender_device)")
        .bind(&id).bind(epoch).bind(device.as_slice()).bind(&actor.device).bind(bytes)
        .bind(&id).bind(epoch).bind(device.as_slice()).bind(actor.person.as_slice())
        .execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    if inserted != 0 {
        app.notify(Some(Event::KeysNeeded(KeyNeed {
            scope,
            epoch,
            device: encode(&device),
        })));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Rejection {
    scope: Scope,
    epoch: i64,
    sender: String,
    envelope: String,
}

pub async fn reject(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Rejection>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Rejection {
        scope,
        epoch,
        sender,
        envelope,
    } = payload(body)?;
    let sender = decode::<32>(&sender)?;
    let envelope = decode_bytes(&envelope)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    read_epoch(&mut tx, &actor, scope).await?;
    let id = encode(&scope.id);
    // A stale rejection must leave any replacement envelope alone.
    let removed = query(
        "DELETE FROM key_envelopes WHERE scope_id = ? AND epoch = ?
        AND recipient_device = ? AND sender_device = ? AND envelope = ?",
    )
    .bind(&id)
    .bind(epoch)
    .bind(&actor.device)
    .bind(sender.as_slice())
    .bind(envelope)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if removed != 0 {
        query(
            "INSERT INTO key_envelope_rejections (scope_id, epoch, recipient_device, sender_person)
            SELECT ?, ?, ?, person_id FROM member_devices WHERE device_id = ?
            ON CONFLICT (scope_id, epoch, recipient_device, sender_person) DO NOTHING",
        )
        .bind(&id)
        .bind(epoch)
        .bind(&actor.device)
        .bind(sender.as_slice())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if removed != 0 {
        app.notify(Some(Event::KeysNeeded(KeyNeed {
            scope,
            epoch,
            device: encode(&actor.device),
        })));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn changed(app: &App, person: &[u8], device: Option<&[u8]>) {
    app.notify(Some(Event::MemberChanged {
        person: encode(person),
        device: device.map(encode),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusttalk_core::identity::DeviceIdentity;

    #[tokio::test]
    async fn device_cap_allows_registered_devices_and_reuses_freed_slots() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        let mut tx = crate::store::write(&db).await.unwrap();
        let (words, first) = DeviceIdentity::create("first").unwrap();
        query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
            .bind(first.certificate.person_id().as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(register_device(&mut tx, &first.certificate).await.unwrap());
        for _ in 1..10 {
            let device = DeviceIdentity::add_device(&words, "other").unwrap();
            assert!(register_device(&mut tx, &device.certificate).await.unwrap());
        }
        assert!(!register_device(&mut tx, &first.certificate).await.unwrap());
        let extra = DeviceIdentity::add_device(&words, "extra").unwrap();
        assert_eq!(
            register_device(&mut tx, &extra.certificate)
                .await
                .unwrap_err()
                .code,
            "device_limit"
        );
        query("DELETE FROM member_devices WHERE device_id = ?")
            .bind(first.certificate.signing_key().as_bytes().as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(register_device(&mut tx, &extra.certificate).await.unwrap());
    }
}
