use std::collections::HashMap;

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode_bytes, encode},
    keys::{Event, Metadata},
};
use serde::{Deserialize, Serialize};
use sqlx::query_as;

use crate::{ApiError, ApiResult, App, auth::authenticate, keys, payload};

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

#[derive(Serialize, Deserialize)]
pub struct Space {
    name_ct: Option<String>,
    description_ct: Option<String>,
    icon_ct: Option<String>,
}

pub async fn space(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Space>> {
    let (mut tx, _) = authenticate(&app, &headers).await?;
    let (name, description, icon): (Ciphertext, Ciphertext, Ciphertext) =
        query_as("SELECT name_ct, description_ct, icon_ct FROM server_settings WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Json(Space {
        name_ct: encoded(name),
        description_ct: encoded(description),
        icon_ct: encoded(icon),
    }))
}

pub async fn update_space(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Space>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Space {
        name_ct,
        description_ct,
        icon_ct,
    } = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    crate::auth::require_owner(&mut tx, &actor.person).await?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    sqlx::query(
        "UPDATE server_settings SET name_ct = ?, description_ct = ?, icon_ct = ? WHERE id = 1",
    )
    .bind(optional(Metadata::ServerName, epoch, name_ct)?)
    .bind(optional(
        Metadata::ServerDescription,
        epoch,
        description_ct,
    )?)
    .bind(optional(Metadata::ServerIcon, epoch, icon_ct)?)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    app.notify(Some(Event::ChannelChanged));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Profile {
    display_name_ct: Option<String>,
    avatar_ct: Option<String>,
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
    sqlx::query("UPDATE members SET display_name_ct = ?, avatar_ct = ? WHERE person_id = ?")
        .bind(optional(Metadata::DisplayName, epoch, display_name_ct)?)
        .bind(optional(Metadata::Avatar, epoch, avatar_ct)?)
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
    display_name_ct: Option<String>,
    avatar_ct: Option<String>,
    role_ids: Vec<String>,
}

pub async fn members(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Vec<Member>>> {
    let (mut tx, _) = authenticate(&app, &headers).await?;
    let rows: Vec<(Vec<u8>, Ciphertext, Ciphertext)> =
        query_as("SELECT person_id, display_name_ct, avatar_ct FROM members ORDER BY person_id")
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
    Ok(Json(
        rows.into_iter()
            .map(|(person, display_name, avatar)| Member {
                person_id: encode(&person),
                display_name_ct: encoded(display_name),
                avatar_ct: encoded(avatar),
                role_ids: roles.remove(&person).unwrap_or_default(),
            })
            .collect(),
    ))
}
