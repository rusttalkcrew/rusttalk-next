use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, encode},
    keys::Metadata,
};
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::{FromRow, SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    keys, metadata, payload,
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
    ManageChannels = 64,
    ManageServer = 128,
    MentionEveryone = 256,
    ManageEmoji = 512,
    MuteMembers = 1024,
    /// Remove people from a voice channel or move them to another.
    MoveMembers = 2048,
}

const PERMISSIONS: [Permission; 12] = [
    Permission::Invite,
    Permission::Kick,
    Permission::Ban,
    Permission::ManageRoles,
    Permission::ViewAuditLog,
    Permission::DeleteMessages,
    Permission::ManageChannels,
    Permission::ManageServer,
    Permission::MentionEveryone,
    Permission::ManageEmoji,
    Permission::MuteMembers,
    Permission::MoveMembers,
];

#[derive(FromRow)]
pub struct Role {
    pub id: String,
    kind: String,
    rank: i64,
    permissions: i64,
    name_ct: Option<Vec<u8>>,
    display_ct: Option<Vec<u8>>,
}

#[derive(Serialize)]
pub struct RoleView {
    id: String,
    kind: String,
    rank: i64,
    permissions: Vec<Permission>,
    name_ct: Option<String>,
    display_ct: Option<String>,
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
            display_ct: role.display_ct.map(|bytes| encode(&bytes)),
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

    pub fn can_set_channel_flags(&self, role: Role) -> ApiResult<()> {
        if self.rank == OWNER_RANK && role.rank == OWNER_RANK {
            return Ok(());
        }
        self.can_grant(role.rank, 0)
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
        "SELECT id, kind, rank, permissions, name_ct, display_ct FROM roles WHERE id IN (
                   SELECT role_id FROM member_roles WHERE person_id = ?
                   UNION ALL SELECT 'owner' FROM server_settings WHERE owner_person_id = ?
                 ) ORDER BY rank DESC, id",
    )
    .bind(person)
    .bind(person)
    .fetch_all(db)
    .await?)
}

pub async fn require_admin(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<()> {
    if !for_person(db, person)
        .await?
        .iter()
        .any(|role| matches!(role.kind.as_str(), "owner" | "admin"))
    {
        return Err(ApiError::forbidden("permission_denied"));
    }
    Ok(())
}

pub async fn authority(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<Authority> {
    let roles = for_person(db, person).await?;
    Ok(Authority {
        rank: roles.first().map_or(-1, |role| role.rank),
        permissions: roles.iter().fold(0, |mask, role| mask | role.permissions),
    })
}

pub async fn by_id(db: &mut SqliteConnection, id: &str) -> ApiResult<Role> {
    query_as("SELECT id, kind, rank, permissions, name_ct, display_ct FROM roles WHERE id = ?")
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
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let default_role_id = query_scalar("SELECT default_role_id FROM server_settings WHERE id = 1")
        .fetch_one(&mut *tx)
        .await?;
    let roles = query_as::<_, Role>(
        "SELECT id, kind, rank, permissions, name_ct, display_ct FROM roles ORDER BY rank DESC, id",
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
    id: Option<String>,
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
        id,
        rank,
        permissions,
        name_ct,
    } = payload(body)?;
    if !(1..OWNER_RANK).contains(&rank) {
        return Err(ApiError::bad_request("invalid_role_rank"));
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
    let id = match id {
        Some(id) => {
            decode::<16>(&id)?;
            id
        }
        None if name_ct.is_none() => encode(&random::<16>()?),
        None => return Err(ApiError::bad_request("role_id_required")),
    };
    let name_ct = match name_ct {
        Some(name) => {
            let epoch = keys::space_epoch(&mut tx, &app).await?;
            Some(metadata::checked(Metadata::RoleName, epoch, name)?)
        }
        None => None,
    };
    let inserted = query(
        "INSERT INTO roles (id, kind, rank, permissions, name_ct) VALUES (?, 'custom', ?, ?, ?)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&id)
    .bind(rank)
    .bind(permissions)
    .bind(&name_ct)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(keys::conflict("role_exists"));
    }
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
                display_ct: None,
            }
            .into(),
        ),
    ))
}

/// Tells a field sent as `null` (`Some(None)`) from one left out (`None`).
fn present<'de, D: Deserializer<'de>>(input: D) -> Result<Option<Option<String>>, D::Error> {
    Option::deserialize(input).map(Some)
}

#[derive(Deserialize)]
pub struct RoleName {
    /// `null` goes back to the built-in name; left out keeps the current one.
    #[serde(default, deserialize_with = "present")]
    name_ct: Option<Option<String>>,
    /// Colour and grouping; `null` clears them, left out keeps them.
    #[serde(default, deserialize_with = "present")]
    display_ct: Option<Option<String>>,
}

pub async fn rename(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<RoleName>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let auth = authority(&mut tx, &actor.person).await?;
    auth.require(Permission::ManageRoles)?;
    let role = by_id(&mut tx, &id).await?;
    // Built-in roles follow the same hierarchy. Only the owner renames the Owner role.
    if auth.rank != OWNER_RANK || role.rank != OWNER_RANK {
        auth.can_grant(role.rank, role.permissions)?;
    }
    let RoleName {
        name_ct,
        display_ct,
    } = payload(body)?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let name = match name_ct {
        Some(name) => metadata::optional(Metadata::RoleName, epoch, name)?,
        None => role.name_ct,
    };
    let display = match display_ct {
        Some(display) => metadata::optional(Metadata::RoleDisplay, epoch, display)?,
        None => role.display_ct,
    };
    query("UPDATE roles SET name_ct = ?, display_ct = ? WHERE id = ?")
        .bind(name)
        .bind(display)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &actor.person,
        "role_renamed",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct RoleEdit {
    permissions: Vec<Permission>,
    /// Omitted keeps the current name.
    name_ct: Option<String>,
    /// Where it sorts; omitted keeps the current rank.
    rank: Option<i64>,
}

pub async fn edit(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<RoleEdit>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let RoleEdit {
        permissions,
        name_ct,
        rank,
    } = payload(body)?;
    if rank.is_some_and(|rank| !(1..OWNER_RANK).contains(&rank)) {
        return Err(ApiError::bad_request("invalid_role_rank"));
    }
    let permissions = permissions
        .into_iter()
        .fold(0, |mask, flag| mask | flag as i64);
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let auth = authority(&mut tx, &actor.person).await?;
    auth.require(Permission::ManageRoles)?;
    let role = by_id(&mut tx, &id).await?;
    // The same checks as deleting the role and making it again: the actor must outrank it,
    // hold what it grants now, and hold everything it will grant.
    auth.can_grant(role.rank, role.permissions)?;
    let rank = rank.unwrap_or(role.rank);
    auth.can_grant(rank, permissions)?;
    if role.kind != "custom" {
        return Err(ApiError::forbidden("built_in_role"));
    }
    let name = match name_ct {
        Some(name) => {
            let epoch = keys::space_epoch(&mut tx, &app).await?;
            Some(metadata::checked(Metadata::RoleName, epoch, name)?)
        }
        None => role.name_ct,
    };
    query("UPDATE roles SET permissions = ?, name_ct = ?, rank = ? WHERE id = ?")
        .bind(permissions)
        .bind(name)
        .bind(rank)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        &actor.person,
        "role_changed",
        None,
        Some(&id),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    // Clients reload roles and their own permissions with the directory.
    app.notify(rusttalk_core::keys::Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
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
    let before = keys::readable_channels(&mut tx, &person).await?;
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
    keys::lost_channels(&mut tx, &person, before).await?;
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
    keys::changed(&app, &person, None);
    crate::voice::enforce(&app, None);
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
    let before = keys::readable_channels(&mut tx, &actor.person).await?;
    query("UPDATE server_settings SET owner_person_id = ? WHERE id = 1")
        .bind(person.as_slice())
        .execute(&mut *tx)
        .await?;
    keys::lost_channels(&mut tx, &actor.person, before).await?;
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
    keys::changed(&app, &actor.person, None);
    keys::changed(&app, &person, None);
    crate::voice::enforce(&app, None);
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Change,
        testing::{app, as_person, person},
    };
    use rusttalk_core::{auth::decode_bytes, keys::Event, presence::Update};

    #[tokio::test]
    async fn editing_a_role_follows_the_grant_rules() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 3).await?;
        // Person 2 manages roles and invites at rank 50; person 3 has no role.
        for (id, rank, permissions) in [
            ("lead", 50, 9),
            ("low", 10, 1),
            ("high", 60, 0),
            ("kickers", 10, 2),
        ] {
            query("INSERT INTO roles (id, kind, rank, permissions) VALUES (?, 'custom', ?, ?)")
                .bind(id)
                .bind(rank)
                .bind(permissions)
                .execute(&app.db)
                .await?;
        }
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'lead')")
            .bind(person(2))
            .execute(&app.db)
            .await?;
        let edit_as = |who: u8, id: &str, permissions: &[Permission]| {
            edit(
                State(app.clone()),
                as_person(who),
                Path(id.to_owned()),
                Ok(Json(RoleEdit {
                    permissions: permissions.to_vec(),
                    name_ct: None,
                    rank: None,
                })),
            )
        };
        let mut events = app.changes.subscribe();
        for (who, id, permissions, code) in [
            (3, "low", &[Permission::Invite][..], "permission_denied"),
            // Granting a permission you don't have.
            (2, "low", &[Permission::Ban], "cannot_grant_permission"),
            (
                2,
                "low",
                &[Permission::MentionEveryone],
                "cannot_grant_permission",
            ),
            // Taking away a permission you don't have.
            (2, "kickers", &[], "cannot_grant_permission"),
            // A role at or above your own.
            (2, "high", &[], "role_hierarchy"),
            (2, "lead", &[Permission::Invite], "role_hierarchy"),
            (1, "moderator", &[], "built_in_role"),
            (1, "member", &[Permission::Invite], "built_in_role"),
            (1, "missing", &[], "role_not_found"),
        ] {
            let refused = edit_as(who, id, permissions).await.unwrap_err();
            assert_eq!(refused.code, code, "{who} editing {id}");
        }
        assert!(events.try_recv().is_err(), "refusals change nothing");

        let saved = edit_as(2, "low", &[Permission::Invite, Permission::ManageRoles]).await;
        assert_eq!(saved.unwrap(), StatusCode::NO_CONTENT);
        let saved = edit_as(
            1,
            "kickers",
            &[Permission::ManageServer, Permission::MentionEveryone],
        )
        .await;
        assert_eq!(saved.unwrap(), StatusCode::NO_CONTENT);
        let stored: Vec<(String, i64)> = query_as(
            "SELECT id, permissions FROM roles WHERE id IN ('low', 'kickers') ORDER BY id",
        )
        .fetch_all(&app.db)
        .await?;
        assert_eq!(stored, [("kickers".into(), 128 | 256), ("low".into(), 9)]);
        let Json(listed) = list(State(app.clone()), as_person(3)).await?;
        for role in listed.roles {
            let flags = serde_json::to_value(role.permissions)?;
            let can_mention = flags
                .as_array()
                .unwrap()
                .iter()
                .any(|flag| flag == "mention_everyone");
            assert_eq!(
                can_mention,
                matches!(role.id.as_str(), "owner" | "admin" | "kickers")
            );
        }
        let audited: i64 = query_scalar(
            "SELECT COUNT(*) FROM audit_log WHERE action = 'role_changed' AND target_id = 'low'",
        )
        .fetch_one(&app.db)
        .await?;
        assert_eq!(audited, 1);
        assert!(matches!(
            events.try_recv(),
            Ok(Change::Update(Update::Event(Event::ChannelChanged)))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn moving_a_role_stays_below_your_own() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        // Person 2 manages roles at rank 50.
        for (id, rank, permissions) in [("lead", 50, 8), ("low", 10, 0)] {
            query("INSERT INTO roles (id, kind, rank, permissions) VALUES (?, 'custom', ?, ?)")
                .bind(id)
                .bind(rank)
                .bind(permissions)
                .execute(&app.db)
                .await?;
        }
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'lead')")
            .bind(person(2))
            .execute(&app.db)
            .await?;
        let move_to = |rank: i64| {
            edit(
                State(app.clone()),
                as_person(2),
                Path("low".to_owned()),
                Ok(Json(RoleEdit {
                    permissions: Vec::new(),
                    name_ct: None,
                    rank: Some(rank),
                })),
            )
        };
        for (rank, code) in [
            (50, "role_hierarchy"),
            (60, "role_hierarchy"),
            (0, "invalid_role_rank"),
            (OWNER_RANK, "invalid_role_rank"),
        ] {
            assert_eq!(move_to(rank).await.unwrap_err().code, code, "rank {rank}");
        }
        assert_eq!(move_to(49).await.unwrap(), StatusCode::NO_CONTENT);
        let rank: i64 = query_scalar("SELECT rank FROM roles WHERE id = 'low'")
            .fetch_one(&app.db)
            .await?;
        assert_eq!(rank, 49);
        Ok(())
    }

    #[tokio::test]
    async fn a_role_display_is_kept_until_sent_again() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 1).await?;
        let space = encode(&keys::space(&app).id);
        query("INSERT INTO key_scopes (id, kind) VALUES (?, 'space') ON CONFLICT DO NOTHING")
            .bind(&space)
            .execute(&app.db)
            .await?;
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(&space)
            .execute(&app.db)
            .await?;
        // Opaque ciphertext: the server only checks its epoch and size.
        let sealed = |fill: u8| encode(&[&1i64.to_be_bytes()[..], &[fill; 40]].concat());
        let rename_as = |body: serde_json::Value| {
            rename(
                State(app.clone()),
                as_person(1),
                Path("moderator".to_owned()),
                Ok(Json(serde_json::from_value(body).unwrap())),
            )
        };
        let stored = || {
            query_as::<_, (Option<Vec<u8>>, Option<Vec<u8>>)>(
                "SELECT name_ct, display_ct FROM roles WHERE id = 'moderator'",
            )
            .fetch_one(&app.db)
        };
        rename_as(serde_json::json!({"display_ct": sealed(1)})).await?;
        assert_eq!(stored().await?, (None, Some(decode_bytes(&sealed(1))?)));
        // Older clients send only the name, which keeps the colour.
        rename_as(serde_json::json!({"name_ct": sealed(2)})).await?;
        assert_eq!(
            stored().await?,
            (
                Some(decode_bytes(&sealed(2))?),
                Some(decode_bytes(&sealed(1))?)
            )
        );
        rename_as(serde_json::json!({"name_ct": null, "display_ct": null})).await?;
        assert_eq!(stored().await?, (None, None));
        let Json(listed) = list(State(app.clone()), as_person(1)).await?;
        assert!(listed.roles.iter().all(|role| role.display_ct.is_none()));
        Ok(())
    }

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
