use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::auth::{decode, encode};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, create_invite},
    payload,
    roles::{self, Permission},
    store::{audit, log_closed},
};

#[derive(Serialize)]
pub struct Me {
    person_id: String,
    device_id: String,
    joined_at: i64,
    role_ids: Vec<String>,
}

pub async fn me(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Me>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let joined_at = query_scalar("SELECT joined_at FROM members WHERE person_id = ?")
        .bind(actor.person.as_slice())
        .fetch_one(&mut *tx)
        .await?;
    let role_ids = roles::for_person(&mut tx, &actor.person)
        .await?
        .into_iter()
        .map(|role| role.id)
        .collect();
    tx.commit().await?;
    Ok(Json(Me {
        person_id: encode(&actor.person),
        device_id: encode(&actor.device),
        joined_at,
        role_ids,
    }))
}

#[derive(Deserialize)]
pub struct InviteRequest {
    uses: i64,
    ttl_seconds: i64,
}

#[derive(Serialize)]
pub struct CreatedInvite {
    id: String,
    code: String,
    expires_at: i64,
}

pub async fn invite(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<InviteRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<CreatedInvite>)> {
    let InviteRequest { uses, ttl_seconds } = payload(body)?;
    if !(1..=1000).contains(&uses) || !(1..=30 * 86_400).contains(&ttl_seconds) {
        return Err(ApiError::bad_request("invalid_invite_limits"));
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::Invite)?;
    let expires_at = actor.at + ttl_seconds;
    query("DELETE FROM invites WHERE expires_at <= ?")
        .bind(actor.at)
        .execute(&mut *tx)
        .await?;
    let (id, code) = create_invite(&mut tx, false, uses, expires_at).await?;
    audit(
        &mut tx,
        &actor.person,
        "invite_created",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedInvite {
            id,
            code,
            expires_at,
        }),
    ))
}

pub async fn revoke_invite(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    decode::<16>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::Invite)?;
    let result = query("DELETE FROM invites WHERE id = ? AND is_owner = FALSE")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("invite_not_found"));
    }
    audit(
        &mut tx,
        &actor.person,
        "invite_revoked",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_member(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<Vec<i64>> {
    let closed = query_scalar("DELETE FROM sessions WHERE person_id = ? RETURNING expires_at")
        .bind(person)
        .fetch_all(&mut *db)
        .await?;
    query("DELETE FROM members WHERE person_id = ?")
        .bind(person)
        .execute(db)
        .await?;
    Ok(closed)
}

pub async fn kick(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
) -> ApiResult<StatusCode> {
    let person = decode::<32>(&person)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let auth = roles::authority(&mut tx, &actor.person).await?;
    auth.require(Permission::Kick)?;
    let target = roles::authority(&mut tx, &person).await?;
    auth.outrank(&target)?;
    if !query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM members WHERE person_id = ?)")
        .bind(person.as_slice())
        .fetch_one(&mut *tx)
        .await?
    {
        return Err(ApiError::not_found("member_not_found"));
    }
    let closed = remove_member(&mut tx, &person).await?;
    audit(
        &mut tx,
        &actor.person,
        "member_kicked",
        Some(&person),
        None,
        actor.at,
    )
    .await?;
    tx.commit().await?;
    log_closed(closed, actor.at, "kicked");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct BanRequest {
    #[serde(default)]
    ban_ip: bool,
}

pub async fn ban(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
    body: Result<Json<BanRequest>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let person = decode::<32>(&person)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let auth = roles::authority(&mut tx, &actor.person).await?;
    auth.require(Permission::Ban)?;
    auth.outrank(&roles::authority(&mut tx, &person).await?)?;
    let added = query("INSERT INTO bans (person_id) VALUES (?) ON CONFLICT (person_id) DO NOTHING")
        .bind(person.as_slice())
        .execute(&mut *tx)
        .await?;
    if added.rows_affected() == 0 {
        return Err(ApiError::new(StatusCode::CONFLICT, "already_banned"));
    }
    if payload(body)?.ban_ip {
        let addresses: Vec<String> =
            query_scalar("SELECT DISTINCT ip FROM sessions WHERE person_id = ? AND expires_at > ?")
                .bind(person.as_slice())
                .bind(actor.at)
                .fetch_all(&mut *tx)
                .await?;
        if addresses.is_empty() {
            return Err(ApiError::new(StatusCode::CONFLICT, "no_active_ip"));
        }
        for ip in addresses {
            let others: Vec<Vec<u8>> = query_scalar("SELECT DISTINCT person_id FROM sessions WHERE ip = ? AND expires_at > ? AND person_id <> ?")
                .bind(&ip).bind(actor.at).bind(person.as_slice()).fetch_all(&mut *tx).await?;
            if ip == actor.ip {
                return Err(ApiError::forbidden("role_hierarchy"));
            }
            for other in others {
                auth.outrank(&roles::authority(&mut tx, &other).await?)?;
            }
            query("INSERT INTO ban_ips (person_id, ip) VALUES (?, ?)")
                .bind(person.as_slice())
                .bind(ip)
                .execute(&mut *tx)
                .await?;
        }
    }
    let closed = remove_member(&mut tx, &person).await?;
    audit(
        &mut tx,
        &actor.person,
        "member_banned",
        Some(&person),
        None,
        actor.at,
    )
    .await?;
    tx.commit().await?;
    log_closed(closed, actor.at, "banned");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unban(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
) -> ApiResult<StatusCode> {
    let person = decode::<32>(&person)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::Ban)?;
    let result = query("DELETE FROM bans WHERE person_id = ?")
        .bind(person.as_slice())
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::not_found("ban_not_found"));
    }
    audit(
        &mut tx,
        &actor.person,
        "member_unbanned",
        Some(&person),
        None,
        actor.at,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    after: i64,
}

#[derive(FromRow)]
struct AuditRow {
    id: i64,
    actor_person_id: Vec<u8>,
    action: String,
    target_person_id: Option<Vec<u8>>,
    target_id: Option<String>,
    at: i64,
}

#[derive(Serialize)]
pub struct AuditEvent {
    id: i64,
    actor_person_id: String,
    action: String,
    target_person_id: Option<String>,
    target_id: Option<String>,
    at: i64,
}

pub async fn audit_log(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> ApiResult<Json<Vec<AuditEvent>>> {
    let after = query
        .map_err(|_| ApiError::bad_request("invalid_query"))?
        .0
        .after;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ViewAuditLog)?;
    let rows = query_as::<_, AuditRow>(
        "SELECT id, actor_person_id, action, target_person_id, target_id, at
                                       FROM audit_log WHERE id > ? ORDER BY id LIMIT 100",
    )
    .bind(after)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| AuditEvent {
                id: row.id,
                actor_person_id: encode(&row.actor_person_id),
                action: row.action,
                target_person_id: row.target_person_id.map(|id| encode(&id)),
                target_id: row.target_id,
                at: row.at,
            })
            .collect(),
    ))
}
