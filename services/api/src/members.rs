use axum::{
    Json,
    extract::{Path, Query, State, rejection::JsonRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{OpenJoining, PROTOCOL_VERSION, ServerInfo, decode, encode},
    keys::{INVITE_BYTES, Metadata},
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{
        PublicLimit, authenticate, authenticate_read, check_public_limit, client_ip, create_invite,
        limit_key,
    },
    keys, metadata, payload,
    roles::{self, Permission},
    store::{audit, hash, log_closed, now, write},
    voice,
};

pub async fn server_info(State(app): State<App>) -> ApiResult<Json<ServerInfo>> {
    let open_joining = query_scalar("SELECT open_joining FROM server_settings WHERE id = 1")
        .fetch_one(&app.db)
        .await?;
    Ok(Json(ServerInfo {
        open_joining,
        version: Some(env!("CARGO_PKG_VERSION").into()),
        protocol: Some(PROTOCOL_VERSION),
    }))
}

pub async fn open_joining(
    State(app): State<App>,
    headers: HeaderMap,
) -> ApiResult<Json<OpenJoining>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    roles::require_admin(&mut tx, &actor.person).await?;
    let (open_joining, member_cap) =
        query_as("SELECT open_joining, member_cap FROM server_settings WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Json(OpenJoining {
        open_joining,
        member_cap,
    }))
}

pub async fn set_open_joining(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<OpenJoining>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::require_admin(&mut tx, &actor.person).await?;
    let OpenJoining {
        open_joining,
        member_cap,
    } = payload(body)?;
    if member_cap < 1 {
        return Err(ApiError::bad_request("invalid_member_cap"));
    }
    let changed = query(
        "UPDATE server_settings SET open_joining = ?, member_cap = ?
        WHERE id = 1 AND (open_joining <> ? OR member_cap <> ?)",
    )
    .bind(open_joining)
    .bind(member_cap)
    .bind(open_joining)
    .bind(member_cap)
    .execute(&mut *tx)
    .await?;
    if changed.rows_affected() > 0 {
        audit(
            &mut tx,
            &actor.person,
            "open_joining_changed",
            None,
            None,
            actor.at,
        )
        .await?;
    }
    tx.commit().await?;
    if changed.rows_affected() > 0 {
        app.listing_changed.send_replace(());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct Me {
    person_id: String,
    device_id: String,
    joined_at: i64,
    role_ids: Vec<String>,
    /// This member's own presence choice; never shown to others.
    status: rusttalk_core::presence::Status,
    hide_offline: bool,
}

pub async fn me(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Me>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    let (joined_at, hide_offline) =
        query_as("SELECT joined_at, hide_offline FROM members WHERE person_id = ?")
            .bind(actor.person.as_slice())
            .fetch_one(&mut *tx)
            .await?;
    let role_ids = roles::for_person(&mut tx, &actor.person)
        .await?
        .into_iter()
        .map(|role| role.id)
        .collect();
    let status = crate::presence::stored(&mut tx, &actor.person).await?;
    tx.commit().await?;
    Ok(Json(Me {
        person_id: encode(&actor.person),
        device_id: encode(&actor.device),
        joined_at,
        role_ids,
        status,
        hide_offline,
    }))
}

#[derive(Deserialize)]
pub struct InviteRequest {
    uses: i64,
    ttl_seconds: i64,
    space_key_ct: String,
    name_ct: Option<String>,
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
    let InviteRequest {
        uses,
        ttl_seconds,
        space_key_ct,
        name_ct,
    } = payload(body)?;
    let blob = decode::<INVITE_BYTES>(&space_key_ct)?;
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
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let name = metadata::optional(Metadata::ServerName, epoch, name_ct)?;
    let has_name: bool =
        query_scalar("SELECT name_ct IS NOT NULL FROM server_settings WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    if has_name && name.is_none() {
        return Err(ApiError::bad_request("invite_name_required"));
    }
    let (id, code) = create_invite(&mut tx, false, uses, expires_at).await?;
    query("UPDATE invites SET space_key_ct = ?, name_ct = ?, created_by = ? WHERE id = ?")
        .bind(blob.as_slice())
        .bind(name)
        .bind(actor.person.as_slice())
        .bind(&id)
        .execute(&mut *tx)
        .await?;
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

#[derive(Deserialize)]
pub struct PreviewRequest {
    code: String,
}

#[derive(Serialize)]
pub struct InvitePreview {
    space_key_ct: String,
    name_ct: Option<String>,
}

pub async fn preview(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<PreviewRequest>, JsonRejection>,
) -> ApiResult<Json<InvitePreview>> {
    let code = decode::<16>(&payload(body)?.code)?;
    let ip = client_ip(&headers)?;
    let mut tx = write(&app.db).await?;
    let at = now()?;
    let ip = check_public_limit(&mut tx, &ip, at, PublicLimit::InvitePreview).await?;
    query("INSERT INTO invite_previews (ip, expires_at) VALUES (?, ?)")
        .bind(ip)
        .bind(at + 60)
        .execute(&mut *tx)
        .await?;
    let result: Option<(Vec<u8>, Option<Vec<u8>>)> = query_as(
        "SELECT space_key_ct, name_ct FROM invites WHERE code_hash = ? AND expires_at > ?
           AND space_key_ct IS NOT NULL",
    )
    .bind(hash(&code).as_slice())
    .bind(at)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    let (space_key, name) = result.ok_or(ApiError::not_found("invite_not_found"))?;
    Ok(Json(InvitePreview {
        space_key_ct: encode(&space_key),
        name_ct: metadata::encoded(name),
    }))
}

#[derive(Debug, Serialize)]
pub struct ActiveInvite {
    id: String,
    /// Null for invites made before creators were recorded.
    created_by: Option<String>,
    remaining_uses: i64,
    expires_at: i64,
}

/// Active invites: your own, or everyone's with Manage server. Codes are stored hashed and
/// never leave the server again.
pub async fn invites(
    State(app): State<App>,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<ActiveInvite>>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    let auth = roles::authority(&mut tx, &actor.person).await?;
    auth.require(Permission::Invite)?;
    let everyone = auth.require(Permission::ManageServer).is_ok();
    let rows: Vec<(String, Option<Vec<u8>>, i64, i64)> = query_as(
        "SELECT id, created_by, remaining_uses, expires_at FROM invites
         WHERE is_owner = FALSE AND expires_at > ? AND (? OR created_by = ?)
         ORDER BY expires_at, id",
    )
    .bind(actor.at)
    .bind(everyone)
    .bind(actor.person.as_slice())
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        rows.into_iter()
            .map(
                |(id, created_by, remaining_uses, expires_at)| ActiveInvite {
                    id,
                    created_by: metadata::encoded(created_by),
                    remaining_uses,
                    expires_at,
                },
            )
            .collect(),
    ))
}

pub async fn revoke_invite(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    decode::<16>(&id)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let auth = roles::authority(&mut tx, &actor.person).await?;
    auth.require(Permission::Invite)?;
    let creator: Option<Option<Vec<u8>>> =
        query_scalar("SELECT created_by FROM invites WHERE id = ? AND is_owner = FALSE")
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
    let creator = creator.ok_or(ApiError::not_found("invite_not_found"))?;
    // Stopping someone else's invite (or an unattributed one) is a server-wide action.
    if creator.as_deref() != Some(actor.person.as_slice()) {
        auth.require(Permission::ManageServer)?;
    }
    query("DELETE FROM invites WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
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

async fn remove_member(app: &App, db: &mut SqliteConnection, person: &[u8]) -> ApiResult<Vec<i64>> {
    let exists: bool = query_scalar("SELECT EXISTS (SELECT 1 FROM members WHERE person_id = ?)")
        .bind(person)
        .fetch_one(&mut *db)
        .await?;
    if exists {
        query("DELETE FROM channels WHERE dm_first = ? OR dm_second = ?")
            .bind(person)
            .bind(person)
            .execute(&mut *db)
            .await?;
        keys::member_removed(app, db, person).await?;
    }
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
    let closed = remove_member(&app, &mut tx, &person).await?;
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
    keys::changed(&app, &person, None);
    voice::enforce(&app, None);
    Ok(StatusCode::NO_CONTENT)
}

/// The caller leaves the server, as if kicked by themselves. The owner must hand over first.
pub async fn leave(State(app): State<App>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let owner: bool =
        query_scalar("SELECT EXISTS (SELECT 1 FROM server_settings WHERE owner_person_id = ?)")
            .bind(actor.person.as_slice())
            .fetch_one(&mut *tx)
            .await?;
    if owner {
        return Err(ApiError::new(StatusCode::CONFLICT, "owner_cannot_leave"));
    }
    let closed = remove_member(&app, &mut tx, &actor.person).await?;
    audit(&mut tx, &actor.person, "left", None, None, actor.at).await?;
    tx.commit().await?;
    log_closed(closed, actor.at, "left");
    keys::changed(&app, &actor.person, None);
    voice::enforce(&app, None);
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
    let added = query(
        "INSERT INTO bans (person_id, banned_at) VALUES (?, ?) ON CONFLICT (person_id) DO NOTHING",
    )
    .bind(person.as_slice())
    .bind(actor.at)
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
        // One IPv6 user owns a whole /64, so the ban covers the prefix.
        let mut banned: Vec<String> = addresses.iter().map(|ip| limit_key(ip)).collect();
        banned.sort();
        banned.dedup();
        if banned.contains(&limit_key(&actor.ip)) {
            return Err(ApiError::forbidden("role_hierarchy"));
        }
        let sessions: Vec<(Vec<u8>, String)> = query_as(
            "SELECT DISTINCT person_id, ip FROM sessions WHERE expires_at > ? AND person_id <> ?",
        )
        .bind(actor.at)
        .bind(person.as_slice())
        .fetch_all(&mut *tx)
        .await?;
        for (other, ip) in sessions {
            if banned.contains(&limit_key(&ip)) {
                auth.outrank(&roles::authority(&mut tx, &other).await?)?;
            }
        }
        for ip in banned {
            query("INSERT INTO ban_ips (person_id, ip) VALUES (?, ?)")
                .bind(person.as_slice())
                .bind(ip)
                .execute(&mut *tx)
                .await?;
        }
    }
    let closed = remove_member(&app, &mut tx, &person).await?;
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
    keys::changed(&app, &person, None);
    voice::enforce(&app, None);
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

#[derive(Serialize)]
pub struct Ban {
    person_id: String,
    banned_at: i64,
    ban_ip: bool,
}

pub async fn bans(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Vec<Ban>>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::Ban)?;
    let rows: Vec<(Vec<u8>, i64, bool)> = query_as(
        "SELECT b.person_id, b.banned_at,
                EXISTS (SELECT 1 FROM ban_ips WHERE person_id = b.person_id)
         FROM bans b ORDER BY b.banned_at DESC, b.person_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        rows.into_iter()
            .map(|(person, banned_at, ban_ip)| Ban {
                person_id: encode(&person),
                banned_at,
                ban_ip,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    after: i64,
    action: Option<String>,
    limit: Option<i64>,
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
    let AuditQuery {
        after,
        action,
        limit,
    } = query.map_err(|_| ApiError::bad_request("invalid_query"))?.0;
    let limit = limit.unwrap_or(100);
    if after < 0 || !(1..=100).contains(&limit) {
        return Err(ApiError::bad_request("invalid_query"));
    }
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ViewAuditLog)?;
    let rows = query_as::<_, AuditRow>(
        "SELECT id, actor_person_id, action, target_person_id, target_id, at
                                       FROM audit_log WHERE id > ? AND (? IS NULL OR action = ?)
                                       AND (dm_channel_id IS NULL OR EXISTS (
                                           SELECT 1 FROM channel_readers r
                                           WHERE r.channel_id = dm_channel_id AND r.person_id = ?
                                       )) ORDER BY id LIMIT ?",
    )
    .bind(after)
    .bind(&action)
    .bind(&action)
    .bind(actor.person.as_slice())
    .bind(limit)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person, person};
    use axum::{body::to_bytes, response::IntoResponse};

    #[tokio::test]
    async fn invites_list_and_revoke_own_or_with_manage_server() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 3).await?;
        // Person 2 may invite; person 3 has no role.
        query(
            "INSERT INTO roles (id, kind, rank, permissions) VALUES ('inviter', 'custom', 10, 1)",
        )
        .execute(&app.db)
        .await?;
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'inviter')")
            .bind(person(2))
            .execute(&app.db)
            .await?;
        let id = |byte: u8| encode(&[byte; 16]);
        let at = now()?;
        // Person 2's, person 1's, an unattributed one, the owner invite and an expired one.
        for (byte, owner, creator, expires_at) in [
            (1, false, Some(person(2)), at + 60),
            (2, false, Some(person(1)), at + 120),
            (3, false, None, at + 180),
            (4, true, None, at + 60),
            (5, false, Some(person(2)), at - 1),
        ] {
            query("INSERT INTO invites (id, code_hash, is_owner, remaining_uses, expires_at, created_by)
                   VALUES (?, ?, ?, 3, ?, ?)")
                .bind(id(byte)).bind(vec![byte; 32]).bind(owner).bind(expires_at).bind(creator)
                .execute(&app.db).await?;
        }
        let listed = |who: u8| invites(State(app.clone()), as_person(who));
        let ids = |list: Json<Vec<ActiveInvite>>| -> Vec<String> {
            list.0.into_iter().map(|invite| invite.id).collect()
        };
        assert_eq!(listed(3).await.unwrap_err().code, "permission_denied");
        let own = listed(2).await.unwrap();
        assert_eq!(own.0[0].created_by, Some(encode(&person(2))));
        assert_eq!(own.0[0].remaining_uses, 3);
        assert_eq!(ids(own), [id(1)]);
        assert_eq!(ids(listed(1).await.unwrap()), [id(1), id(2), id(3)]);

        let revoke =
            |who: u8, byte: u8| revoke_invite(State(app.clone()), as_person(who), Path(id(byte)));
        for (who, byte, code) in [
            (3, 1, "permission_denied"),
            (2, 2, "permission_denied"),
            (2, 3, "permission_denied"),
            (1, 4, "invite_not_found"),
        ] {
            assert_eq!(
                revoke(who, byte).await.unwrap_err().code,
                code,
                "{who} stopping {byte}"
            );
        }
        assert_eq!(revoke(2, 1).await.unwrap(), StatusCode::NO_CONTENT);
        assert_eq!(revoke(1, 2).await.unwrap(), StatusCode::NO_CONTENT);
        assert_eq!(revoke(1, 3).await.unwrap(), StatusCode::NO_CONTENT);
        assert_eq!(revoke(1, 1).await.unwrap_err().code, "invite_not_found");
        assert!(ids(listed(1).await.unwrap()).is_empty());
        let audited: i64 =
            query_scalar("SELECT COUNT(*) FROM audit_log WHERE action = 'invite_revoked'")
                .fetch_one(&app.db)
                .await?;
        assert_eq!(audited, 3);
        Ok(())
    }

    #[tokio::test]
    async fn server_info_reports_version_protocol_and_admission() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        let app = App {
            emoji: crate::emoji::Emoji::open(dir.path(), &db)
                .await
                .unwrap()
                .into(),
            files: crate::files::Files::open(dir.path(), &db)
                .await
                .unwrap()
                .into(),
            db,
            key: ed25519_dalek::SigningKey::from_bytes(&[7; 32]).into(),
            changes: tokio::sync::broadcast::channel(128).0,
            presence: Default::default(),
            voice: None,
            session_ttl: crate::auth::SESSION_TTL,
            listing_changed: tokio::sync::watch::channel(()).0,
        };
        for open_joining in [false, true] {
            query("UPDATE server_settings SET open_joining = ?")
                .bind(open_joining)
                .execute(&app.db)
                .await
                .unwrap();
            let response = server_info(State(app.clone()))
                .await
                .unwrap()
                .into_response();
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                serde_json::json!({
                    "open_joining": open_joining,
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol": PROTOCOL_VERSION
                })
            );
        }
    }
}
