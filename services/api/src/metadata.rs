use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode_bytes, encode},
    keys::{Event, Metadata},
    presence::Presence,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{SqlitePool, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    keys, payload,
    roles::{self, Permission},
};

pub const IMAGE_BODY_LIMIT: usize = 356 * 1024;

type Ciphertext = Option<Vec<u8>>;

pub fn checked(kind: Metadata, epoch: i64, input: String) -> ApiResult<Vec<u8>> {
    let bytes = decode_bytes(&input)?;
    let supplied = kind
        .epoch(&bytes)
        .map_err(|_| ApiError::bad_request("invalid_metadata"))?;
    if supplied != epoch {
        return Err(keys::conflict("metadata_epoch_changed"));
    }
    Ok(bytes)
}

pub fn optional(kind: Metadata, epoch: i64, input: Option<String>) -> ApiResult<Option<Vec<u8>>> {
    input.map(|value| checked(kind, epoch, value)).transpose()
}

pub fn encoded(bytes: Option<Vec<u8>>) -> Option<String> {
    bytes.map(|value| encode(&value))
}

/// A short hash of picture ciphertext, so clients refetch a picture only when it changes.
pub(crate) fn picture_hash(ciphertext: &[u8]) -> String {
    encode(&Sha256::digest(ciphertext)[..16])
}

/// Hash pictures stored before hashes were kept.
pub async fn hash_pictures(db: &SqlitePool) -> anyhow::Result<()> {
    let avatars: Vec<(Vec<u8>, Vec<u8>)> = query_as(
        "SELECT person_id, avatar_ct FROM members
         WHERE avatar_ct IS NOT NULL AND avatar_hash IS NULL",
    )
    .fetch_all(db)
    .await?;
    for (person, avatar) in avatars {
        query("UPDATE members SET avatar_hash = ? WHERE person_id = ?")
            .bind(picture_hash(&avatar))
            .bind(person)
            .execute(db)
            .await?;
    }
    query(
        "UPDATE server_settings SET icon_hash = ? WHERE id = 1 AND icon_hash IS NULL
         AND icon_ct IS NOT NULL",
    )
    .bind(
        query_scalar::<_, Option<Vec<u8>>>("SELECT icon_ct FROM server_settings WHERE id = 1")
            .fetch_optional(db)
            .await?
            .flatten()
            .map(|icon| picture_hash(&icon)),
    )
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Serialize)]
pub struct Space {
    name_ct: Option<String>,
    description_ct: Option<String>,
    icon_hash: Option<String>,
}

pub async fn space(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Space>> {
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let (name, description, icon_hash): (Ciphertext, Ciphertext, Option<String>) =
        query_as("SELECT name_ct, description_ct, icon_hash FROM server_settings WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Json(Space {
        name_ct: encoded(name),
        description_ct: encoded(description),
        icon_hash,
    }))
}

pub async fn icon(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let icon: Ciphertext = query_scalar("SELECT icon_ct FROM server_settings WHERE id = 1")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    let icon = icon.ok_or_else(|| ApiError::not_found("icon_not_found"))?;
    Ok(Json(json!({ "icon_ct": encode(&icon) })))
}

pub async fn avatar(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
) -> ApiResult<Json<Value>> {
    let person = decode_bytes(&person)?;
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let avatar: Option<Ciphertext> =
        query_scalar("SELECT avatar_ct FROM members WHERE person_id = ?")
            .bind(person)
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    let avatar = avatar
        .flatten()
        .ok_or_else(|| ApiError::not_found("avatar_not_found"))?;
    Ok(Json(json!({ "avatar_ct": encode(&avatar) })))
}

// An omitted field stays unchanged. An explicit null clears it.
fn patch_field<'de, D: Deserializer<'de>>(input: D) -> Result<Option<Option<String>>, D::Error> {
    Option::deserialize(input).map(Some)
}

#[derive(Deserialize)]
pub struct SpacePatch {
    #[serde(default, deserialize_with = "patch_field")]
    name_ct: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch_field")]
    description_ct: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch_field")]
    icon_ct: Option<Option<String>>,
}

pub async fn update_space(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<SpacePatch>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let SpacePatch {
        name_ct,
        description_ct,
        icon_ct,
    } = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageServer)?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let icon = optional(Metadata::ServerIcon, epoch, icon_ct.clone().flatten())?;
    sqlx::query(
        "UPDATE server_settings SET
            name_ct = CASE WHEN ? THEN ? ELSE name_ct END,
            description_ct = CASE WHEN ? THEN ? ELSE description_ct END,
            icon_ct = CASE WHEN ? THEN ? ELSE icon_ct END,
            icon_hash = CASE WHEN ? THEN ? ELSE icon_hash END
         WHERE id = 1",
    )
    .bind(name_ct.is_some())
    .bind(optional(Metadata::ServerName, epoch, name_ct.flatten())?)
    .bind(description_ct.is_some())
    .bind(optional(
        Metadata::ServerDescription,
        epoch,
        description_ct.flatten(),
    )?)
    .bind(icon_ct.is_some())
    .bind(icon.as_deref())
    .bind(icon_ct.is_some())
    .bind(icon.as_deref().map(picture_hash))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    app.notify(Event::ChannelChanged);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Profile {
    #[serde(default, deserialize_with = "patch_field")]
    display_name_ct: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch_field")]
    avatar_ct: Option<Option<String>>,
}

pub async fn update_profile(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Profile>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Profile {
        display_name_ct,
        avatar_ct,
    } = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let avatar = optional(Metadata::Avatar, epoch, avatar_ct.clone().flatten())?;
    sqlx::query(
        "UPDATE members SET
            display_name_ct = CASE WHEN ? THEN ? ELSE display_name_ct END,
            avatar_ct = CASE WHEN ? THEN ? ELSE avatar_ct END,
            avatar_hash = CASE WHEN ? THEN ? ELSE avatar_hash END
         WHERE person_id = ?",
    )
    .bind(display_name_ct.is_some())
    .bind(optional(
        Metadata::DisplayName,
        epoch,
        display_name_ct.flatten(),
    )?)
    .bind(avatar_ct.is_some())
    .bind(avatar.as_deref())
    .bind(avatar_ct.is_some())
    .bind(avatar.as_deref().map(picture_hash))
    .bind(actor.person.as_slice())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    keys::changed(&app, &actor.person, None);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct Member {
    person_id: String,
    presence: Presence,
    display_name_ct: Option<String>,
    avatar_hash: Option<String>,
    role_ids: Vec<String>,
    /// Clients leave this member out of member lists while they are offline.
    hide_offline: bool,
}

pub async fn members(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Vec<Member>>> {
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let rows: Vec<(Vec<u8>, Ciphertext, Option<String>, bool)> = query_as(
        "SELECT person_id, display_name_ct, avatar_hash, hide_offline FROM members
         ORDER BY person_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let assignments: Vec<(Vec<u8>, String)> = query_as(
        "SELECT person_id, role_id FROM member_roles
         UNION ALL SELECT owner_person_id, 'owner' FROM server_settings WHERE owner_person_id IS NOT NULL
         ORDER BY role_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut roles = HashMap::<_, Vec<_>>::new();
    for (person, role) in assignments {
        roles.entry(person).or_default().push(role);
    }
    tx.commit().await?;
    let presence = app.presence.snapshot();
    Ok(Json(
        rows.into_iter()
            .map(|(person, display_name, avatar_hash, hide_offline)| {
                let person_id = encode(&person);
                Member {
                    presence: presence.get(&person_id).copied().unwrap_or_default(),
                    person_id,
                    display_name_ct: encoded(display_name),
                    avatar_hash,
                    role_ids: roles.remove(&person).unwrap_or_default(),
                    hide_offline,
                }
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pictures_stored_before_hashes_get_one_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::store::open(dir.path()).await.unwrap();
        query("INSERT INTO members (person_id, joined_at, avatar_ct) VALUES (?, 1, ?)")
            .bind([3; 32].as_slice())
            .bind([9; 64].as_slice())
            .execute(&db)
            .await
            .unwrap();
        hash_pictures(&db).await.unwrap();
        let hash: Option<String> = query_scalar("SELECT avatar_hash FROM members")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(hash, Some(picture_hash(&[9; 64])));
        assert_eq!(hash.unwrap().len(), 22);
    }

    #[tokio::test]
    async fn hidden_members_are_flagged_and_show_presence_once_back() -> anyhow::Result<()> {
        use crate::testing::{app, as_person};
        use rusttalk_core::presence::Status;
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let hide = |on: bool| {
            crate::presence::set_hide_offline(
                State(app.clone()),
                as_person(2),
                Ok(Json(
                    serde_json::from_value(json!({ "hide_offline": on })).unwrap(),
                )),
            )
        };
        let listed = || async {
            let Json(rows) = members(State(app.clone()), as_person(1)).await.unwrap();
            let row = serde_json::to_value(&rows[1]).unwrap();
            (row["hide_offline"].clone(), row["presence"].clone())
        };
        assert_eq!(listed().await, (json!(false), json!("offline")));

        assert_eq!(hide(true).await?, StatusCode::NO_CONTENT);
        let Json(me) = crate::members::me(State(app.clone()), as_person(2)).await?;
        assert_eq!(serde_json::to_value(me)?["hide_offline"], true);
        assert_eq!(listed().await, (json!(true), json!("offline")));

        let person = encode(&crate::testing::person(2));
        let _socket = crate::presence::open(&app, person, Status::Online);
        assert_eq!(listed().await, (json!(true), json!("online")));

        hide(false).await?;
        assert_eq!(listed().await.0, json!(false));
        Ok(())
    }
}
