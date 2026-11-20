use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::auth::{decode, decode_bytes, encode};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::authenticate,
    payload,
    store::{audit, random},
};

const OWNER_RANK: i64 = 1000;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Invite = 1,
    Kick = 2,
    Ban = 4,
    ManageRoles = 8,
    ViewAuditLog = 16,
    DeleteMessages = 32,
}

const PERMISSIONS: [Permission; 6] = [
    Permission::Invite,
    Permission::Kick,
    Permission::Ban,
    Permission::ManageRoles,
    Permission::ViewAuditLog,
    Permission::DeleteMessages,
];

#[derive(FromRow)]
pub struct Role {
    pub id: String,
    kind: String,
    rank: i64,
    permissions: i64,
    name_ct: Option<Vec<u8>>,
}

#[derive(Serialize)]
pub struct RoleView {
    id: String,
    kind: String,
    rank: i64,
    permissions: Vec<Permission>,
    name_ct: Option<String>,
}

impl From<Role> for RoleView {
    fn from(role: Role) -> Self {
        Self {
            id: role.id,
            kind: role.kind,
            rank: role.rank,
            permissions: PERMISSIONS
                .into_iter()
                .filter(|flag| role.permissions & *flag as i64 != 0)
                .collect(),
            name_ct: role.name_ct.map(|bytes| encode(&bytes)),
        }
    }
}

pub struct Authority {
    rank: i64,
    permissions: i64,
}

impl Authority {
    pub fn require(&self, flag: Permission) -> ApiResult<()> {
        if self.permissions & flag as i64 == 0 {
            return Err(ApiError::forbidden("permission_denied"));
        }
        Ok(())
    }

    pub fn outrank(&self, target: &Authority) -> ApiResult<()> {
        if target.rank >= OWNER_RANK || target.rank >= self.rank {
            return Err(ApiError::forbidden("role_hierarchy"));
        }
        Ok(())
    }

    fn can_grant(&self, rank: i64, permissions: i64) -> ApiResult<()> {
        self.outrank(&Authority { rank, permissions })?;
        if permissions & !self.permissions != 0 {
            return Err(ApiError::forbidden("cannot_grant_permission"));
        }
        Ok(())
    }
}

pub async fn for_person(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<Vec<Role>> {
    Ok(query_as(
        "SELECT id, kind, rank, permissions, name_ct FROM roles WHERE id IN (
                   SELECT role_id FROM member_roles WHERE person_id = ?
                   UNION ALL SELECT 'owner' FROM server_settings WHERE owner_person_id = ?
                 ) ORDER BY rank DESC, id",
    )
    .bind(person)
    .bind(person)
    .fetch_all(db)
    .await?)
}

pub async fn authority(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<Authority> {
    let roles = for_person(db, person).await?;
    Ok(Authority {
        rank: roles.first().map_or(-1, |role| role.rank),
        permissions: roles.iter().fold(0, |mask, role| mask | role.permissions),
    })
}

async fn by_id(db: &mut SqliteConnection, id: &str) -> ApiResult<Role> {
    query_as("SELECT id, kind, rank, permissions, name_ct FROM roles WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(ApiError::not_found("role_not_found"))
}

#[derive(Serialize)]
pub struct RoleList {
    default_role_id: String,
    roles: Vec<RoleView>,
}

pub async fn list(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<RoleList>> {
    let (mut tx, _) = authenticate(&app, &headers).await?;
    let default_role_id = query_scalar("SELECT default_role_id FROM server_settings WHERE id = 1")
        .fetch_one(&mut *tx)
        .await?;
    let roles = query_as::<_, Role>(
        "SELECT id, kind, rank, permissions, name_ct FROM roles ORDER BY rank DESC, id",
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(RoleView::from)
    .collect();
    tx.commit().await?;
    Ok(Json(RoleList {
        default_role_id,
        roles,
    }))
}

#[derive(Deserialize)]
pub struct NewRole {
    rank: i64,
    permissions: Vec<Permission>,
    name_ct: Option<String>,
}

pub async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewRole>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<RoleView>)> {
    let NewRole {
        rank,
        permissions,
        name_ct,
    } = payload(body)?;
    if !(1..OWNER_RANK).contains(&rank) {
        return Err(ApiError::bad_request("invalid_role_rank"));
    }
    let name_ct = name_ct.as_deref().map(decode_bytes).transpose()?;
    if name_ct.as_ref().is_some_and(|name| name.len() > 512) {
        return Err(ApiError::bad_request("role_name_too_large"));
    }
    let permissions = permissions
        .into_iter()
        .fold(0, |mask, flag| mask | flag as i64);

    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let auth = authority(&mut tx, &actor.person).await?;
    auth.require(Permission::ManageRoles)?;
    auth.can_grant(rank, permissions)?;
    let count: i64 = query_scalar("SELECT COUNT(*) FROM roles")
        .fetch_one(&mut *tx)
        .await?;
    if count >= 128 {
        return Err(ApiError::new(StatusCode::CONFLICT, "role_limit"));
    }
    let id = encode(&random::<16>()?);
    query("INSERT INTO roles (id, kind, rank, permissions, name_ct) VALUES (?, 'custom', ?, ?, ?)")
        .bind(&id)
        .bind(rank)
        .bind(permissions)
        .bind(&name_ct)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &actor.person,
        "role_created",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(
            Role {
                id,
                kind: "custom".into(),
                rank,
                permissions,
                name_ct,
            }
            .into(),
        ),
    ))
}

pub async fn delete(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let auth = authority(&mut tx, &actor.person).await?;
    auth.require(Permission::ManageRoles)?;
    let role = by_id(&mut tx, &id).await?;
    auth.can_grant(role.rank, role.permissions)?;
    if role.kind != "custom" {
        return Err(ApiError::forbidden("built_in_role"));
    }
    let used: bool = query_scalar("SELECT EXISTS (SELECT 1 FROM member_roles WHERE role_id = ?)
                                      OR EXISTS (SELECT 1 FROM server_settings WHERE default_role_id = ?)")
        .bind(&id).bind(&id).fetch_one(&mut *tx).await?;
    if used {
        return Err(ApiError::new(StatusCode::CONFLICT, "role_in_use"));
    }
    query("DELETE FROM roles WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &actor.person,
        "role_deleted",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct DefaultRole {
    role_id: String,
}

pub async fn set_default(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<DefaultRole>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let auth = authority(&mut tx, &actor.person).await?;
    auth.require(Permission::ManageRoles)?;
    let id = payload(body)?.role_id;
    let role = by_id(&mut tx, &id).await?;
    auth.can_grant(role.rank, role.permissions)?;
    query("UPDATE server_settings SET default_role_id = ? WHERE id = 1")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &actor.person,
        "default_role_changed",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Assignment {
    role_ids: Vec<String>,
}

pub async fn assign(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
    body: Result<Json<Assignment>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let person = decode::<32>(&person)?;
    let mut ids = payload(body)?.role_ids;
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() || ids.len() > 128 {
        return Err(ApiError::bad_request("invalid_role_count"));
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let auth = authority(&mut tx, &actor.person).await?;
    auth.require(Permission::ManageRoles)?;
    let target = authority(&mut tx, &person).await?;
    if target.rank < 0 {
        return Err(ApiError::not_found("member_not_found"));
    }
    auth.outrank(&target)?;
    for id in &ids {
        let role = by_id(&mut tx, id).await?;
        auth.can_grant(role.rank, role.permissions)?;
    }
    query("DELETE FROM member_roles WHERE person_id = ?")
        .bind(person.as_slice())
        .execute(&mut *tx)
        .await?;
    for id in &ids {
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, ?)")
            .bind(person.as_slice())
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    audit(
        &mut tx,
        &actor.person,
        "member_roles_changed",
        Some(&person),
        None,
        actor.at,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct NewOwner {
    person_id: String,
}

pub async fn transfer(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewOwner>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    if authority(&mut tx, &actor.person).await?.rank != OWNER_RANK {
        return Err(ApiError::forbidden("owner_required"));
    }
    let person = decode::<32>(&payload(body)?.person_id)?;
    if person == actor.person {
        return Err(ApiError::bad_request("already_owner"));
    }
    let target = authority(&mut tx, &person).await?;
    if target.rank < 0 {
        return Err(ApiError::not_found("member_not_found"));
    }
    query("UPDATE server_settings SET owner_person_id = ? WHERE id = 1")
        .bind(person.as_slice())
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &actor.person,
        "ownership_transferred",
        Some(&person),
        None,
        actor.at,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_protects_owner_and_peers_and_caps_grants() {
        let owner = Authority {
            rank: OWNER_RANK,
            permissions: 63,
        };
        let admin = Authority {
            rank: 100,
            permissions: 63,
        };
        let mod_ = Authority {
            rank: 50,
            permissions: 38,
        };
        assert!(owner.outrank(&admin).is_ok());
        assert!(owner.outrank(&owner).is_err());
        assert!(admin.outrank(&owner).is_err());
        assert!(admin.outrank(&admin).is_err());
        assert!(admin.outrank(&mod_).is_ok());
        assert!(admin.can_grant(100, 0).is_err());
        assert!(mod_.can_grant(49, Permission::ManageRoles as i64).is_err());
        assert!(mod_.can_grant(49, Permission::Kick as i64).is_ok());
    }
}
