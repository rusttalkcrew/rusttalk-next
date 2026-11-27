use std::{collections::HashMap, env, sync::Mutex};

use anyhow::Context;
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::decode,
    identity::PersonId,
    keys::{Event, Kind, Scope},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{query, query_scalar};

use crate::{
    ApiError, ApiResult, App, Change, auth::authenticate, keys, limits, payload, store::now,
};

/// New DM conversations one person may start on this server per hour. Enough for
/// someone joining a server full of people they already talk to (each needs a copy
/// here), too few to message a whole server. Old DMs reopened don't count.
const NEW_PER_HOUR: usize = 20;
const HOUR: i64 = 3600;

/// When each person last started new DMs here. Kept in memory only, so the server
/// stores nothing new about who writes to whom; a restart forgets it.
pub struct Starts {
    limit: usize,
    started: Mutex<HashMap<PersonId, Vec<i64>>>,
}

impl Default for Starts {
    fn default() -> Self {
        Self::new(NEW_PER_HOUR)
    }
}

impl Starts {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            started: Mutex::default(),
        }
    }

    /// The limit, or a smaller one for development stacks so end-to-end tests can reach it.
    pub fn from_env() -> anyhow::Result<Self> {
        if env::var("RUSTTALK_DEV").as_deref() == Ok("1")
            && let Ok(value) = env::var("RUSTTALK_DEV_NEW_DMS_PER_HOUR")
            && !value.is_empty()
        {
            return Ok(Self::new(value.parse().context("invalid dev DM limit")?));
        }
        Ok(Self::default())
    }

    /// Count a new DM by `person` at `at` (Unix seconds), or refuse it over the limit.
    fn take(&self, person: PersonId, at: i64) -> ApiResult<()> {
        // Nothing panics while it is held, so a poisoned map is still consistent.
        let mut started = (self.started.lock()).unwrap_or_else(std::sync::PoisonError::into_inner);
        started.retain(|_, times| {
            times.retain(|time| *time > at - HOUR);
            !times.is_empty()
        });
        let times = started.entry(person).or_default();
        if times.len() >= self.limit {
            return Err(ApiError::new(StatusCode::TOO_MANY_REQUESTS, "new_dm_limit"));
        }
        times.push(at);
        Ok(())
    }
}

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
    app.dm_starts.take(actor.person, now()?)?;
    // Only a new conversation counts: reopening one is free.
    app.limits.check(&actor, limits::Action::NewDm)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_dms_are_limited_per_person_and_hour() {
        let starts = Starts::new(2);
        let (ana, bo) = ([1; 32], [2; 32]);
        starts.take(ana, 100).unwrap();
        starts.take(ana, 200).unwrap();
        let refused = starts.take(ana, 300).unwrap_err();
        assert_eq!(
            (refused.status, refused.code),
            (StatusCode::TOO_MANY_REQUESTS, "new_dm_limit")
        );
        // Someone else has their own hour.
        starts.take(bo, 300).unwrap();
        // An hour after the first one, there is room for one more.
        starts.take(ana, 100 + HOUR).unwrap();
        assert!(starts.take(ana, 150 + HOUR).is_err());
    }
}
