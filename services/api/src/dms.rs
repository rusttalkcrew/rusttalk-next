use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::decode,
    keys::{Event, Kind, Scope},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{query, query_scalar};

use crate::{ApiError, ApiResult, App, Change, auth::authenticate, keys, payload};

#[derive(Deserialize)]
pub struct NewDm {
    id: String,
    peer: String,
    commitment: String,
    envelope: String,
}

pub async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewDm>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let NewDm {
        id,
        peer,
        commitment,
        envelope,
    } = payload(body)?;
    let scope = Scope {
        kind: Kind::Channel,
        id: decode(&id)?,
    };
    let peer = decode::<32>(&peer)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    if peer == actor.person {
        return Err(ApiError::bad_request("invalid_dm_peer"));
    }
    if !query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM members WHERE person_id = ?)")
        .bind(peer.as_slice())
        .fetch_one(&mut *tx)
        .await?
    {
        return Err(ApiError::not_found("member_not_found"));
    }
    let mut pair = [actor.person, peer];
    pair.sort_unstable();
    let existing: Option<String> =
        query_scalar("SELECT id FROM channels WHERE dm_first = ? AND dm_second = ?")
            .bind(pair[0].as_slice())
            .bind(pair[1].as_slice())
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(id) = existing {
        tx.commit().await?;
        return Ok((StatusCode::OK, Json(json!({ "id": id }))));
    }
    let reserved = query("INSERT INTO channel_ids (id) VALUES (?) ON CONFLICT (id) DO NOTHING")
        .bind(&id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if reserved == 0 || scope.id == keys::space(&app).id {
        return Err(keys::conflict("channel_exists"));
    }
    query(
        "INSERT INTO channels (id, kind, position, dm_first, dm_second) VALUES (?, 'dm', 0, ?, ?)",
    )
    .bind(&id)
    .bind(pair[0].as_slice())
    .bind(pair[1].as_slice())
    .execute(&mut *tx)
    .await?;
    keys::start_channel(
        &mut tx,
        &app,
        &actor,
        scope,
        decode(&commitment)?,
        &envelope,
    )
    .await?;
    tx.commit().await?;
    // Only the two people gain a scope; the notice reaches only sockets that can read it.
    app.signal(Change::Access(pair));
    app.notify(Event::DmCreated {
        channel: id.clone(),
    });
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}
