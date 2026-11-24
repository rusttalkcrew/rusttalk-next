use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, decode_bytes, encode},
    identity::DeviceCertificate,
    keys::{
        EnvelopeContext, Event, Excluded, KeyNeed, KeyState, Kind, MAX_INLINE_EVENT_BYTES,
        MemberDevice, MemberProfile, Scope, ScopeState, WrappedKey,
    },
};
use serde::Deserialize;
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App, Change,
    auth::{Actor, authenticate, authenticate_read},
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
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
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
    excluded: String,
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
        "SELECT k.id, k.kind, COALESCE(MAX(e.epoch), 0) AS epoch, k.rotate, COALESCE((SELECT excluded FROM key_epochs WHERE scope_id=k.id ORDER BY epoch DESC LIMIT 1), '[]') AS excluded
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
                excluded: exclusions(&row.excluded)?,
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
    excluded: String,
    id: String,
    kind: String,
    epoch: i64,
    sender_device: Vec<u8>,
    commitment: Vec<u8>,
    envelope: Vec<u8>,
}

fn exclusions(json: &str) -> ApiResult<Excluded> {
    serde_json::from_str(json).map_err(|_| ApiError::internal("invalid_stored_exclusions"))
}

fn stored_commitment(bytes: Vec<u8>) -> ApiResult<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| ApiError::internal("invalid_stored_commitment"))
}

pub async fn state(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<KeyState>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    let rows: Vec<EnvelopeRow> = query_as(
        "SELECT k.id, k.kind, e.epoch, e.sender_device, v.commitment, v.excluded, e.envelope FROM key_envelopes e
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
                excluded: exclusions(&row.excluded)?,
            })
        })
        .collect::<ApiResult<_>>()?;
    let records: Vec<(Vec<u8>, Vec<u8>)> = query_as(
        "SELECT r.person_id, r.record FROM revocations r JOIN members m ON m.person_id = r.person_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let state = KeyState {
        supports_exclusions: true,
        revocations: records
            .into_iter()
            .map(|(person, record)| (encode(&person), encode(&record)))
            .collect(),
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

pub async fn read_epoch(
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

pub async fn start_channel(
    db: &mut SqliteConnection,
    app: &App,
    actor: &Actor,
    scope: Scope,
    commitment: [u8; 32],
    envelope: &str,
) -> ApiResult<()> {
    let id = encode(&scope.id);
    query("INSERT INTO key_scopes (id, kind, channel_id) VALUES (?, 'channel', ?)")
        .bind(&id)
        .bind(&id)
        .execute(&mut *db)
        .await?;
    let context = EnvelopeContext {
        server_id: space(app).id,
        scope,
        epoch: 1,
        commitment,
    };
    start_epoch(db, actor, context, envelope, &Rotation::default()).await
}

#[derive(Default, Deserialize)]
struct Rotation {
    #[serde(default)]
    excluded: Excluded,
    #[serde(default)]
    revocations: Vec<(String, String)>,
}

/// Only a root-signed removal for a reader can authorize an unscheduled epoch.
/// Existing exclusions survive later rotations, including permission changes.
async fn check_rotation(
    db: &mut SqliteConnection,
    scope: Scope,
    current: i64,
    rotation: &Rotation,
) -> ApiResult<bool> {
    let id = encode(&scope.id);
    let previous: Option<String> =
        query_scalar("SELECT excluded FROM key_epochs WHERE scope_id = ? AND epoch = ?")
            .bind(&id)
            .bind(current)
            .fetch_optional(&mut *db)
            .await?;
    let mut expected = exclusions(previous.as_deref().unwrap_or("[]"))?;
    let before = expected.len();
    if rotation.excluded.len() > 4096 {
        return Err(conflict("revocation_limit"));
    }
    if rotation.revocations.len() > 4096 {
        return Err(ApiError::bad_request("invalid_revocation"));
    }
    if !expected.is_subset(&rotation.excluded) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "epoch_exclusions_required",
        ));
    }
    if rotation.excluded == expected {
        return Ok(false);
    }
    if rotation.revocations.len() != rotation.excluded.len().saturating_sub(before) {
        return Err(conflict("epoch_changed"));
    }
    for (owner, signed) in &rotation.revocations {
        let owner = decode::<32>(owner)?;
        let record = rusttalk_core::identity::Revocation::verify(owner, &decode_bytes(signed)?)
            .map_err(|_| ApiError::bad_request("invalid_revocation"))?;
        let reader: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM scope_readers WHERE scope_id=? AND person_id=?)",
        )
        .bind(&id)
        .bind(owner.as_slice())
        .fetch_one(&mut *db)
        .await?;
        if !reader {
            return Err(ApiError::forbidden("recipient_read_denied"));
        }
        let registered: bool = query_scalar(
            "SELECT EXISTS(SELECT 1 FROM member_devices WHERE person_id=?1 AND device_id=?2
             UNION ALL SELECT 1 FROM message_senders WHERE person_id=?1 AND device_id=?2
             UNION ALL SELECT 1 FROM revocations WHERE person_id=?1 AND device_id=?2 AND was_registered)",
        ).bind(owner.as_slice()).bind(record.device_signing_key().as_slice()).fetch_one(&mut *db).await?;
        if !registered {
            return Err(ApiError::bad_request("unknown_removed_device"));
        }
        expected.insert(rusttalk_core::keys::exclusion_id(
            &owner,
            record.device_signing_key(),
        ));
    }
    // One root cannot spend other members' slots by naming their public device keys.
    let mut counts = std::collections::HashMap::new();
    for device in &expected {
        let owner = device
            .split_once('.')
            .ok_or(ApiError::bad_request("invalid_revocation"))?
            .0;
        let count = counts.entry(owner).or_insert(0);
        *count += 1;
        if *count > rusttalk_core::identity::REVOCATIONS_PER_PERSON {
            return Err(conflict("revocation_limit"));
        }
    }
    if expected != rotation.excluded {
        return Err(conflict("epoch_changed"));
    }
    Ok(expected.len() > before)
}

async fn start_epoch(
    db: &mut SqliteConnection,
    actor: &Actor,
    context: EnvelopeContext,
    envelope: &str,
    rotation: &Rotation,
) -> ApiResult<()> {
    let EnvelopeContext {
        scope,
        epoch,
        commitment,
        ..
    } = context;
    let (current, rotate) = read_epoch(db, actor, scope).await?;
    if current.checked_add(1) != Some(epoch) {
        return Err(conflict("epoch_changed"));
    }
    // read_epoch already checked that the actor reads this scope, so for a DM they are one of its two people.
    let dm = scope.kind == Kind::Channel
        && query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM channels WHERE id = ? AND kind = 'dm')",
        )
        .bind(encode(&scope.id))
        .fetch_one(&mut *db)
        .await?;
    if rotation
        .excluded
        .contains(&rusttalk_core::keys::exclusion_id(
            &actor.person,
            &actor.device,
        ))
    {
        return Err(ApiError::forbidden("device_revoked"));
    }
    let removing = check_rotation(db, scope, current, rotation).await?;
    if !may_start_epoch(current, epoch, rotate || removing, dm) {
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
    query("INSERT INTO key_epochs (scope_id, epoch, commitment, excluded) VALUES (?, ?, ?, ?)")
        .bind(&id)
        .bind(epoch)
        .bind(commitment.as_slice())
        .bind(serde_json::to_string(&rotation.excluded).expect("string set serializes"))
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

/// The next epoch can start when the scope has none yet or needs a rotation. Either
/// person in a DM may also start one at any time: their app re-keys a DM once it has
/// pinned who it is with, so no key shared before then can read later messages.
fn may_start_epoch(current: i64, epoch: i64, rotate: bool, dm: bool) -> bool {
    current.checked_add(1) == Some(epoch) && (current == 0 || rotate || dm)
}

#[derive(Deserialize)]
pub struct NewEpoch {
    #[serde(flatten)]
    rotation: Rotation,
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
        rotation,
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
    start_epoch(&mut tx, &actor, context, &envelope, &rotation).await?;
    tx.commit().await?;
    app.signal(Change::Keys);
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
    let recipient: Vec<u8> = query_scalar("SELECT d.person_id FROM member_devices d JOIN scope_readers r ON d.person_id = r.person_id WHERE d.device_id = ? AND r.scope_id = ?")
        .bind(device.as_slice()).bind(&id).fetch_optional(&mut *tx).await?.ok_or(ApiError::forbidden("recipient_read_denied"))?;
    let (commitment, excluded): (Vec<u8>, String) =
        query_as("SELECT commitment, excluded FROM key_epochs WHERE scope_id = ? AND epoch = ?")
            .bind(&id)
            .bind(epoch)
            .fetch_one(&mut *tx)
            .await?;
    let excluded = exclusions(&excluded)?;
    if excluded.contains(&rusttalk_core::keys::exclusion_id(&recipient, &device))
        || excluded.contains(&rusttalk_core::keys::exclusion_id(
            &actor.person,
            &actor.device,
        ))
    {
        return Err(ApiError::forbidden("recipient_read_denied"));
    }
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
        app.notify(Event::KeysNeeded(KeyNeed {
            scope,
            epoch,
            device: encode(&device),
        }));
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
        app.notify(Event::KeysNeeded(KeyNeed {
            scope,
            epoch,
            device: encode(&actor.device),
        }));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub fn changed(app: &App, person: &[u8], device: Option<&[u8]>) {
    app.notify(Event::MemberChanged {
        person: encode(person),
        device: device.map(encode),
        profile: None,
    });
}

/// Tell clients only this member's profile changed, so they update that member without
/// reloading keys or the directory. Without a profile it is a plain member change.
pub fn profile_changed(app: &App, person: &[u8], profile: Option<MemberProfile>) {
    app.notify(profile_event(person, profile));
}

fn profile_event(person: &[u8], profile: Option<MemberProfile>) -> Event {
    let event = Event::MemberChanged {
        person: encode(person),
        device: None,
        profile: profile.map(Box::new),
    };
    // A notice too big for older apps' event frames goes without the profile.
    match serde_json::to_vec(&event) {
        Ok(bytes) if bytes.len() <= MAX_INLINE_EVENT_BYTES => event,
        _ => Event::MemberChanged {
            person: encode(person),
            device: None,
            profile: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusttalk_core::identity::DeviceIdentity;

    #[test]
    fn a_profile_too_big_for_older_apps_is_left_out() {
        let profile = |len: usize| MemberProfile {
            display_name_ct: Some("a".repeat(len)),
            avatar_hash: None,
            bio_ct: None,
            hide_offline: false,
            avatar_initialized: true,
            voice_muted: false,
        };
        let inline = |event: Event| match event {
            Event::MemberChanged { profile, .. } => profile.is_some(),
            _ => unreachable!(),
        };
        assert!(inline(profile_event(&[1; 32], Some(profile(1500)))));
        assert!(!inline(profile_event(&[1; 32], Some(profile(4000)))));
    }

    #[test]
    fn a_dm_can_be_rekeyed_by_its_people_but_a_channel_only_when_flagged() {
        assert!(may_start_epoch(0, 1, false, false));
        assert!(may_start_epoch(3, 4, true, false));
        assert!(!may_start_epoch(3, 4, false, false));
        assert!(may_start_epoch(3, 4, false, true));
        // Still only the next epoch.
        assert!(!may_start_epoch(3, 5, false, true));
        assert!(!may_start_epoch(3, 3, true, true));
    }

    #[tokio::test]
    async fn only_a_new_signed_removal_for_a_reader_authorizes_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        let mut tx = crate::store::write(&db).await.unwrap();
        let (words, device) = DeviceIdentity::create("device").unwrap();
        let (other, stranger) = DeviceIdentity::create("other").unwrap();
        let scope = Scope {
            kind: Kind::Space,
            id: [7; 32],
        };
        init_space(&mut tx, &scope.id).await.unwrap();
        query("INSERT INTO members (person_id, joined_at) VALUES (?,0)")
            .bind(device.certificate.person_id().as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        let signing = device.certificate.signing_key().to_bytes();
        let proof = |phrase, person| {
            encode(
                &rusttalk_core::identity::revoke_device(phrase, person, signing)
                    .unwrap()
                    .to_bytes(),
            )
        };
        let mut request = Rotation {
            excluded: Excluded::from([rusttalk_core::keys::certificate_id(&device.certificate)]),
            revocations: vec![(
                encode(device.certificate.person_id()),
                proof(&words, device.certificate.person_id()),
            )],
        };
        assert_eq!(
            check_rotation(&mut tx, scope, 0, &request)
                .await
                .unwrap_err()
                .code,
            "unknown_removed_device"
        );
        register_device(&mut tx, &device.certificate).await.unwrap();
        assert!(check_rotation(&mut tx, scope, 0, &request).await.unwrap());
        query("INSERT INTO revocations (person_id,device_id,record) VALUES (?,?,?)")
            .bind(device.certificate.person_id().as_slice())
            .bind(signing.as_slice())
            .bind(decode_bytes(&request.revocations[0].1).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        query("DELETE FROM member_devices WHERE device_id=?")
            .bind(signing.as_slice())
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(check_rotation(&mut tx, scope, 0, &request).await.unwrap());
        query("INSERT INTO key_epochs (scope_id,epoch,commitment,excluded) VALUES (?,1,zeroblob(32),?)").bind(encode(&scope.id)).bind(serde_json::to_string(&request.excluded).unwrap()).execute(&mut *tx).await.unwrap();
        assert!(!check_rotation(&mut tx, scope, 1, &request).await.unwrap());
        assert_eq!(
            check_rotation(&mut tx, scope, 1, &Rotation::default())
                .await
                .unwrap_err()
                .status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        request.revocations[0].1 = proof(&other, stranger.certificate.person_id());
        assert!(check_rotation(&mut tx, scope, 0, &request).await.is_err());
        request.revocations[0].0 = encode(stranger.certificate.person_id());
        assert!(check_rotation(&mut tx, scope, 0, &request).await.is_err());
    }

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
