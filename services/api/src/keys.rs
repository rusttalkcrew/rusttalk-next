use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, decode_bytes, encode},
    identity::DeviceCertificate,
    keys::{
        Event, KeyNeed, KeyState, Kind, MemberDevice, Scope, ScopeState, WrappedKey,
        verify_envelope,
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
    let inserted = query(
        "INSERT INTO member_devices (device_id, person_id, certificate)
         VALUES (?, ?, ?) ON CONFLICT (device_id) DO NOTHING",
    )
    .bind(device)
    .bind(cert.person_id().as_slice())
    .bind(certificate)
    .execute(&mut *db)
    .await?
    .rows_affected()
        != 0;
    if !inserted {
        let matches: bool = query_scalar(
            "SELECT EXISTS (SELECT 1 FROM member_devices
              WHERE device_id = ? AND person_id = ? AND certificate = ?)",
        )
        .bind(device)
        .bind(cert.person_id().as_slice())
        .bind(certificate)
        .fetch_one(db)
        .await?;
        if !matches {
            return Err(conflict("device_certificate_changed"));
        }
    }
    Ok(inserted)
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

type EnvelopeRow = (String, String, i64, Vec<u8>, Vec<u8>);

pub async fn state(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<KeyState>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let rows: Vec<EnvelopeRow> = query_as(
        "SELECT k.id, k.kind, e.epoch, e.sender_device, e.envelope FROM key_envelopes e
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
        .map(|(id, kind, epoch, sender, envelope)| {
            Ok(WrappedKey {
                need: KeyNeed {
                    scope: scope(&id, &kind)?,
                    epoch,
                    device: encode(&actor.device),
                },
                sender: encode(&sender),
                envelope: encode(&envelope),
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
    scope: Scope,
    epoch: i64,
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
    verify_envelope(scope, epoch, device, &sender, &bytes)
        .map_err(|_| ApiError::bad_request("invalid_envelope"))?;
    Ok(bytes)
}

pub async fn start_epoch(
    db: &mut SqliteConnection,
    actor: &Actor,
    scope: Scope,
    epoch: i64,
    envelope: &str,
) -> ApiResult<()> {
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
    let bytes = checked_envelope(db, actor, scope, epoch, device, envelope).await?;
    let id = encode(&scope.id);
    query("INSERT INTO key_epochs (scope_id, epoch) VALUES (?, ?)")
        .bind(&id)
        .bind(epoch)
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
        envelope,
    } = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    start_epoch(&mut tx, &actor, scope, epoch, &envelope).await?;
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
    let bytes = checked_envelope(&mut tx, &actor, scope, epoch, &device, &envelope).await?;
    let inserted = query("INSERT INTO key_envelopes (scope_id, epoch, recipient_device, sender_device, envelope)
        VALUES (?, ?, ?, ?, ?) ON CONFLICT (scope_id, epoch, recipient_device) DO UPDATE
        SET sender_device = excluded.sender_device, envelope = excluded.envelope
        WHERE NOT EXISTS (SELECT 1 FROM member_devices WHERE device_id = key_envelopes.sender_device)")
        .bind(id).bind(epoch).bind(device.as_slice()).bind(&actor.device).bind(bytes)
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

pub fn changed(app: &App, person: &[u8], device: Option<&[u8]>) {
    app.notify(Some(Event::MemberChanged {
        person: encode(person),
        device: device.map(encode),
    }));
}
