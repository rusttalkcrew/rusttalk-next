//! Lockdown (#665): while it's on, nobody new joins, by invite or open joining. A spike of
//! joins can turn it on by itself; the settings say how many joins in how long.

use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard, PoisonError},
};

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    index::{open_settings, seal_settings},
    keys::{Event, Part},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{SqliteConnection, query, query_as};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    payload,
    roles::{self, Permission},
    store::audit_details,
};

/// The longest message shown to people turned away, in characters.
const MAX_MESSAGE_CHARS: usize = 500;
/// Automatic lockdown counts at most this many joins.
const MAX_JOINS: i64 = 1000;
/// The time window joins are counted in, in seconds.
const WINDOW_SECS: std::ops::RangeInclusive<i64> = 60..=3600;

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct Settings {
    locked: bool,
    /// Shown to people turned away. Empty or missing for none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// Joins within `auto_secs` that turn lockdown on; 0 is off.
    auto_joins: i64,
    auto_secs: i64,
    /// The join-spike detector turned it on, not a person. Ignored when saving.
    #[serde(default)]
    automatic: bool,
}

type Row = (bool, bool, Option<Vec<u8>>, i64, i64);

async fn stored(app: &App, db: &mut SqliteConnection) -> ApiResult<Settings> {
    let (locked, automatic, message, auto_joins, auto_secs): Row = query_as(
        "SELECT locked_down, locked_automatically, lockdown_message_ct, auto_lockdown_joins,
                auto_lockdown_secs
         FROM server_settings WHERE id = 1",
    )
    .fetch_one(db)
    .await?;
    Ok(Settings {
        locked,
        message: message.map(|ct| open(app, &ct)).transpose()?,
        auto_joins,
        auto_secs,
        automatic,
    })
}

fn open(app: &App, ciphertext: &[u8]) -> ApiResult<String> {
    let clear = open_settings(&app.key, ciphertext)
        .map_err(|_| ApiError::internal("lockdown_message_decrypt"))?;
    String::from_utf8(clear).map_err(|_| ApiError::internal("lockdown_message_decrypt"))
}

pub async fn get(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Settings>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::LockDown)?;
    let settings = stored(&app, &mut tx).await?;
    tx.commit().await?;
    Ok(Json(settings))
}

pub async fn set(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Settings>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::LockDown)?;
    let mut new = payload(body)?;
    new.message = new.message.filter(|text| !text.trim().is_empty());
    if new
        .message
        .as_ref()
        .is_some_and(|text| text.chars().count() > MAX_MESSAGE_CHARS)
    {
        return Err(ApiError::bad_request("invalid_lockdown_message"));
    }
    if !(new.auto_joins == 0 || (2..=MAX_JOINS).contains(&new.auto_joins))
        || !WINDOW_SECS.contains(&new.auto_secs)
    {
        return Err(ApiError::bad_request("invalid_auto_lockdown"));
    }
    let old = stored(&app, &mut tx).await?;
    // Saving keeps who turned it on, unless this save turns it on or off.
    new.automatic = new.locked && old.locked && old.automatic;
    if new == old {
        tx.commit().await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    let message = (new.message.as_deref())
        .map(|text| seal_settings(&app.key, text.as_bytes()))
        .transpose()
        .map_err(|_| ApiError::internal("lockdown_message_encrypt"))?;
    query(
        "UPDATE server_settings SET locked_down = ?, locked_automatically = ?,
             lockdown_message_ct = ?, auto_lockdown_joins = ?, auto_lockdown_secs = ?
         WHERE id = 1",
    )
    .bind(new.locked)
    .bind(new.automatic)
    .bind(message)
    .bind(new.auto_joins)
    .bind(new.auto_secs)
    .execute(&mut *tx)
    .await?;
    // The message is left out, as for the public listing: only that it changed.
    let mut changes = Vec::new();
    if new.locked != old.locked {
        let action = if new.locked {
            "lockdown_started"
        } else {
            "lockdown_ended"
        };
        changes.push((action, None));
    }
    if new.message != old.message {
        changes.push(("lockdown_message_changed", None));
    }
    if (new.auto_joins, new.auto_secs) != (old.auto_joins, old.auto_secs) {
        let details = json!({
            "before": { "joins": old.auto_joins, "secs": old.auto_secs },
            "after": { "joins": new.auto_joins, "secs": new.auto_secs },
        });
        changes.push(("auto_lockdown_changed", Some(details)));
    }
    for (action, details) in changes {
        audit_details(
            &mut tx,
            &actor.person,
            action,
            None,
            None,
            details,
            actor.at,
        )
        .await?;
    }
    tx.commit().await?;
    if new.locked != old.locked {
        tracing::info!(locked = new.locked, "lockdown changed");
    }
    // Recent joins count again from a lift: the spike that caused it doesn't lock again.
    if !new.locked {
        app.joins.list().clear();
    }
    changed(&app);
    Ok(StatusCode::NO_CONTENT)
}

fn changed(app: &App) {
    app.notify(Event::ChannelChanged {
        part: Some(Part::Lockdown),
    });
}

/// Refuse someone new while the server is locked down, with its message if there is one.
pub async fn check(app: &App, db: &mut SqliteConnection) -> ApiResult<()> {
    let (locked, message): (bool, Option<Vec<u8>>) =
        query_as("SELECT locked_down, lockdown_message_ct FROM server_settings WHERE id = 1")
            .fetch_one(&mut *db)
            .await?;
    if !locked {
        return Ok(());
    }
    // Turned away all the same if the message can't be read.
    let message = message.and_then(|ct| open(app, &ct).ok());
    Err(ApiError {
        message,
        ..ApiError::forbidden("server_locked")
    })
}

/// Times of the latest joins, at most `auto_lockdown_joins` of them. In memory: a restart
/// only starts the count again.
#[derive(Default)]
pub struct Joins(Mutex<VecDeque<i64>>);

impl Joins {
    fn list(&self) -> MutexGuard<'_, VecDeque<i64>> {
        // Nothing panics halfway through a change, so the list is consistent after a
        // poisoning panic.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Count `person` joining at `at`, and lock the server down when that makes too many joins
/// too quickly. True when it did; the caller sends the change once the join is committed.
pub async fn joined(
    app: &App,
    db: &mut SqliteConnection,
    person: &[u8],
    at: i64,
) -> ApiResult<bool> {
    let (locked, limit, secs): (bool, i64, i64) = query_as(
        "SELECT locked_down, auto_lockdown_joins, auto_lockdown_secs
         FROM server_settings WHERE id = 1",
    )
    .fetch_one(&mut *db)
    .await?;
    if limit == 0 || !spike(&mut app.joins.list(), limit, secs, at) || locked {
        return Ok(false);
    }
    query(
        "UPDATE server_settings SET locked_down = TRUE, locked_automatically = TRUE WHERE id = 1",
    )
    .execute(&mut *db)
    .await?;
    // The person whose join tipped it over is the actor: nobody chose it.
    audit_details(
        db,
        person,
        "lockdown_auto_started",
        None,
        None,
        Some(json!({ "joins": limit, "secs": secs })),
        at,
    )
    .await?;
    tracing::warn!(joins = limit, secs, "join spike: server locked down");
    Ok(true)
}

/// Add a join at `at` and say whether `limit` joins now fall within `secs`.
fn spike(joins: &mut VecDeque<i64>, limit: i64, secs: i64, at: i64) -> bool {
    joins.push_back(at);
    while joins.len() as i64 > limit {
        joins.pop_front();
    }
    joins.len() as i64 == limit && joins.front().is_some_and(|first| at - first < secs)
}

/// Send the change after a join locked the server down.
pub fn auto_locked(app: &App) {
    app.joins.list().clear();
    changed(app);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person};

    fn settings(locked: bool, message: Option<&str>, joins: i64, secs: i64) -> Settings {
        Settings {
            locked,
            message: message.map(str::to_owned),
            auto_joins: joins,
            auto_secs: secs,
            automatic: false,
        }
    }

    #[test]
    fn ten_joins_in_two_minutes_is_a_spike() {
        let mut joins = VecDeque::new();
        // Nine joins a minute apart never get there.
        for minute in 0..9 {
            assert!(!spike(&mut joins, 10, 120, minute * 60));
        }
        let mut joins = VecDeque::new();
        for second in 0..9 {
            assert!(!spike(&mut joins, 10, 120, 1000 + second * 10));
        }
        assert!(spike(&mut joins, 10, 120, 1119));
        assert!(joins.len() == 10);
        // Ten spread over more than two minutes is not.
        let mut joins = VecDeque::new();
        for second in 0..10 {
            assert!(!spike(&mut joins, 10, 120, second * 14));
        }
    }

    #[tokio::test]
    async fn lockdown_needs_the_permission_and_is_audited() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 3).await?;
        query(
            "INSERT INTO member_roles (person_id, role_id) VALUES (?, 'member'), (?, 'moderator')",
        )
        .bind(crate::testing::person(2))
        .bind(crate::testing::person(3))
        .execute(&app.db)
        .await?;
        let save = |who: u8, settings: Settings| {
            set(State(app.clone()), as_person(who), Ok(Json(settings)))
        };
        let code = |result: ApiResult<StatusCode>| result.unwrap_err().code;

        // Ten joins in two minutes, on, until someone changes it.
        let Json(before) = get(State(app.clone()), as_person(1)).await?;
        assert_eq!(before, settings(false, None, 10, 120));
        assert_eq!(
            code(save(2, settings(true, None, 10, 120)).await),
            "permission_denied"
        );
        assert_eq!(
            code(save(3, settings(true, None, 1, 120)).await),
            "invalid_auto_lockdown"
        );
        assert_eq!(
            code(save(3, settings(true, None, 10, 5)).await),
            "invalid_auto_lockdown"
        );
        let long = "x".repeat(MAX_MESSAGE_CHARS + 1);
        assert_eq!(
            code(save(3, settings(true, Some(&long), 10, 120)).await),
            "invalid_lockdown_message"
        );

        // A moderator locks it with a message, which is sealed at rest.
        save(3, settings(true, Some("Back tomorrow"), 0, 120)).await?;
        let Json(now) = get(State(app.clone()), as_person(3)).await?;
        assert_eq!(now, settings(true, Some("Back tomorrow"), 0, 120));
        let sealed: Vec<u8> = sqlx::query_scalar("SELECT lockdown_message_ct FROM server_settings")
            .fetch_one(&app.db)
            .await?;
        assert!(!sealed.windows(4).any(|part| part == b"Back"));

        // Joining is refused with that message.
        let mut db = app.db.acquire().await?;
        let refused = check(&app, &mut db).await.unwrap_err();
        assert_eq!(
            (refused.status, refused.code),
            (StatusCode::FORBIDDEN, "server_locked")
        );
        assert_eq!(refused.message.as_deref(), Some("Back tomorrow"));
        drop(db);

        save(1, settings(false, None, 0, 120)).await?;
        let actions: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT action, details FROM audit_log ORDER BY id")
                .fetch_all(&app.db)
                .await?;
        let auto = json!({
            "before": { "joins": 10, "secs": 120 },
            "after": { "joins": 0, "secs": 120 },
        });
        assert_eq!(
            actions,
            [
                ("lockdown_started".into(), None),
                ("lockdown_message_changed".into(), None),
                ("auto_lockdown_changed".into(), Some(auto.to_string())),
                ("lockdown_ended".into(), None),
                ("lockdown_message_changed".into(), None),
            ]
        );
        let mut db = app.db.acquire().await?;
        check(&app, &mut db).await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_join_spike_locks_the_server_once() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 1).await?;
        let mut db = app.db.acquire().await?;
        query("UPDATE server_settings SET auto_lockdown_joins = 3")
            .execute(&mut *db)
            .await?;
        assert!(!joined(&app, &mut db, &[2; 32], 100).await?);
        assert!(!joined(&app, &mut db, &[3; 32], 110).await?);
        assert!(joined(&app, &mut db, &[4; 32], 120).await?);
        auto_locked(&app);
        let Json(now) = get(State(app.clone()), as_person(1)).await?;
        assert!(now.locked && now.automatic);
        // Already locked: nothing more to do.
        for (byte, at) in [(5, 121), (6, 122), (7, 123)] {
            assert!(!joined(&app, &mut db, &[byte; 32], at).await?);
        }
        let audited: Vec<(Vec<u8>, String)> =
            sqlx::query_as("SELECT actor_person_id, action FROM audit_log")
                .fetch_all(&mut *db)
                .await?;
        assert_eq!(audited, [(vec![4; 32], "lockdown_auto_started".into())]);
        Ok(())
    }
}
