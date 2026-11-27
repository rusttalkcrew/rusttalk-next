//! Timeouts (#663): a moderator stops a lower-ranked member from writing in channels,
//! reacting, creating invites and joining or talking in voice for a while; they can still
//! read and listen. The end is stored, so a timeout outlasts restarts and rejoining, and
//! every check compares it with the clock, so it ends on its own.

use std::time::Duration;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{auth::decode, keys::Metadata};
use serde::Deserialize;
use serde_json::json;
use sqlx::{SqliteConnection, query, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::authenticate,
    keys, metadata, payload,
    roles::{self, Permission},
    store::{self, audit_details},
    voice,
};

/// The lengths apps offer, in seconds: 60 s, 5 min, 10 min, 1 h, 1 day and 1 week.
pub const LENGTHS: [i64; 6] = [60, 300, 600, 3600, 86_400, 604_800];

/// When this person's timeout ends, while one runs at `now`.
pub async fn ends_at(db: &mut SqliteConnection, person: &[u8], now: i64) -> ApiResult<Option<i64>> {
    Ok(
        query_scalar("SELECT ends_at FROM timeouts WHERE person_id = ? AND ends_at > ?")
            .bind(person)
            .bind(now)
            .fetch_optional(db)
            .await?,
    )
}

/// Refuse with `403 timed_out` while this person is in a timeout.
pub async fn refuse(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<()> {
    match ends_at(db, person, store::now()?).await? {
        Some(_) => Err(ApiError::forbidden("timed_out")),
        None => Ok(()),
    }
}

/// The running timeout's end and reason, for the member's own `/v1/me`.
pub async fn own(
    db: &mut SqliteConnection,
    person: &[u8],
    now: i64,
) -> ApiResult<Option<(i64, Option<Vec<u8>>)>> {
    Ok(sqlx::query_as(
        "SELECT ends_at, reason_ct FROM timeouts WHERE person_id = ? AND ends_at > ?",
    )
    .bind(person)
    .bind(now)
    .fetch_optional(db)
    .await?)
}

#[derive(Deserialize)]
pub struct NewTimeout {
    seconds: i64,
    /// Space-key ciphertext, `Metadata::TimeoutReason` under the member's person ID.
    #[serde(default)]
    reason_ct: Option<String>,
}

/// The actor may time out `person`: `timeout_members` and a strictly higher rank, as for
/// a kick, and they are a member.
async fn require(db: &mut SqliteConnection, actor: &[u8], person: &[u8]) -> ApiResult<()> {
    let auth = roles::authority(db, actor).await?;
    auth.require(Permission::TimeoutMembers)?;
    auth.outrank(&roles::authority(db, person).await?)?;
    if !query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM members WHERE person_id = ?)")
        .bind(person)
        .fetch_one(&mut *db)
        .await?
    {
        return Err(ApiError::not_found("member_not_found"));
    }
    Ok(())
}

pub async fn set(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
    body: Result<Json<NewTimeout>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let NewTimeout { seconds, reason_ct } = payload(body)?;
    let person = decode::<32>(&person)?;
    if !LENGTHS.contains(&seconds) {
        return Err(ApiError::bad_request("invalid_timeout"));
    }
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    require(&mut tx, &actor.person, &person).await?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let reason = metadata::optional(Metadata::TimeoutReason, epoch, reason_ct)?;
    let ends_at = actor.at + seconds;
    query("DELETE FROM timeouts WHERE ends_at <= ?")
        .bind(actor.at)
        .execute(&mut *tx)
        .await?;
    // A new timeout replaces a running one, so a moderator can shorten or lengthen it.
    query(
        "INSERT INTO timeouts (person_id, ends_at, reason_ct) VALUES (?, ?, ?)
         ON CONFLICT (person_id) DO UPDATE SET ends_at = excluded.ends_at, reason_ct = excluded.reason_ct",
    )
    .bind(person.as_slice())
    .bind(ends_at)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    audit_details(
        &mut tx,
        &actor.person,
        "member_timed_out",
        Some(&person),
        None,
        Some(json!({ "seconds": seconds, "ends_at": ends_at })),
        actor.at,
    )
    .await?;
    let profile = metadata::profile(&mut tx, &person).await?;
    tx.commit().await?;
    // The member's app shows its banner, and member lists their timeout, right away.
    keys::profile_changed(&app, &person, profile);
    voice::enforce_person(&app, &person);
    give_back_at(&app, person.to_vec(), ends_at);
    Ok(StatusCode::NO_CONTENT)
}

/// End a running timeout early.
pub async fn end(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
) -> ApiResult<StatusCode> {
    let person = decode::<32>(&person)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    require(&mut tx, &actor.person, &person).await?;
    let ended: Option<i64> =
        query_scalar("DELETE FROM timeouts WHERE person_id = ? RETURNING ends_at")
            .bind(person.as_slice())
            .fetch_optional(&mut *tx)
            .await?;
    if !ended.is_some_and(|ends_at| ends_at > actor.at) {
        return Err(ApiError::not_found("timeout_not_found"));
    }
    audit_details(
        &mut tx,
        &actor.person,
        "member_timeout_ended",
        Some(&person),
        None,
        None,
        actor.at,
    )
    .await?;
    let profile = metadata::profile(&mut tx, &person).await?;
    tx.commit().await?;
    keys::profile_changed(&app, &person, profile);
    voice::enforce_person(&app, &person);
    Ok(StatusCode::NO_CONTENT)
}

/// Give a person their voice back once their timeout ends: LiveKit keeps the grants it was
/// last told until told again. A timeout ended or replaced meanwhile makes this a no-op check.
fn give_back_at(app: &App, person: Vec<u8>, ends_at: i64) {
    if app.voice.is_none() {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        let wait = ends_at
            .saturating_sub(store::now().unwrap_or(ends_at))
            .max(0);
        // One second late, so the check runs after the end and not on its edge.
        tokio::time::sleep(Duration::from_secs(wait as u64 + 1)).await;
        voice::enforce_person(&app, &person);
    });
}

/// At start, schedule the end of every running timeout (see `give_back_at`).
pub async fn resume(app: &App) -> ApiResult<()> {
    let running: Vec<(Vec<u8>, i64)> =
        sqlx::query_as("SELECT person_id, ends_at FROM timeouts WHERE ends_at > ?")
            .bind(store::now()?)
            .fetch_all(&app.db)
            .await?;
    for (person, ends_at) in running {
        give_back_at(app, person, ends_at);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person, person};
    use rusttalk_core::auth::encode;

    /// Person 2 moderates, 3 is a member, 4 an admin.
    async fn server(dir: &std::path::Path) -> anyhow::Result<App> {
        let app = app(dir, 4).await?;
        for (who, role) in [(2, "moderator"), (3, "member"), (4, "admin")] {
            query("INSERT INTO member_roles (person_id, role_id) VALUES (?, ?)")
                .bind(person(who))
                .bind(role)
                .execute(&app.db)
                .await?;
        }
        let mut db = app.db.acquire().await?;
        keys::init_space(&mut db, &keys::space(&app).id).await?;
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(encode(&keys::space(&app).id))
            .execute(&mut *db)
            .await?;
        Ok(app)
    }

    fn timeout(seconds: i64) -> Result<Json<NewTimeout>, JsonRejection> {
        Ok(Json(NewTimeout {
            seconds,
            reason_ct: None,
        }))
    }

    #[tokio::test]
    async fn a_timeout_follows_the_hierarchy_is_stored_audited_and_ends_early() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let app = server(dir.path()).await?;
        let put = |who: u8, target: u8, seconds: i64| {
            set(
                State(app.clone()),
                as_person(who),
                Path(encode(&person(target))),
                timeout(seconds),
            )
        };
        for (who, target, seconds, code) in [
            (3, 2, 60, "permission_denied"),
            (2, 4, 60, "role_hierarchy"),
            (2, 1, 60, "role_hierarchy"),
            (2, 2, 60, "role_hierarchy"),
            (4, 9, 60, "member_not_found"),
            (2, 3, 61, "invalid_timeout"),
        ] {
            let refused = put(who, target, seconds).await.unwrap_err();
            assert_eq!(refused.code, code, "{who} timing out {target}");
        }

        let mut events = app.changes.subscribe();
        assert_eq!(put(2, 3, 600).await?, StatusCode::NO_CONTENT);
        assert!(
            events.try_recv().is_ok(),
            "the member's app hears it at once"
        );
        let mut db = app.db.acquire().await?;
        let now = store::now()?;
        let ends = ends_at(&mut db, &person(3), now).await?.unwrap();
        assert!((now + 599..=now + 600).contains(&ends));
        assert_eq!(
            refuse(&mut db, &person(3)).await.unwrap_err().code,
            "timed_out"
        );
        refuse(&mut db, &person(2)).await?;
        let profile = metadata::profile(&mut db, &person(3)).await?.unwrap();
        assert_eq!(profile.timeout_until, Some(ends));
        // It ends on its own: past its end nothing is refused.
        assert_eq!(ends_at(&mut db, &person(3), ends).await?, None);
        drop(db);

        let lift = |who: u8| end(State(app.clone()), as_person(who), Path(encode(&person(3))));
        assert_eq!(lift(3).await.unwrap_err().code, "permission_denied");
        assert_eq!(lift(4).await?, StatusCode::NO_CONTENT);
        assert_eq!(lift(4).await.unwrap_err().code, "timeout_not_found");
        let mut db = app.db.acquire().await?;
        refuse(&mut db, &person(3)).await?;
        let audited: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT action, details FROM audit_log WHERE target_person_id = ? ORDER BY id",
        )
        .bind(person(3))
        .fetch_all(&mut *db)
        .await?;
        let details = json!({ "seconds": 600, "ends_at": ends }).to_string();
        assert_eq!(
            audited,
            [
                ("member_timed_out".into(), Some(details)),
                ("member_timeout_ended".into(), None)
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_timeout_outlasts_leaving_and_blocks_writes_and_invites() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = server(dir.path()).await?;
        query("UPDATE roles SET permissions = permissions | 1 WHERE id = 'member'")
            .execute(&app.db)
            .await?;
        query("INSERT INTO categories (id, position, name_ct) VALUES ('cat', 0, x'00')")
            .execute(&app.db)
            .await?;
        let (channel, room) = (encode(&[5; 32]), encode(&[6; 32]));
        for (id, kind) in [(&channel, "text"), (&room, "voice")] {
            query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, ?, 'cat', 0, x'00')")
                .bind(id)
                .bind(kind)
                .execute(&app.db)
                .await?;
            query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES (?, 'member', 15)")
                .bind(id)
                .execute(&app.db)
                .await?;
        }
        set(
            State(app.clone()),
            as_person(2),
            Path(encode(&person(3))),
            timeout(60),
        )
        .await?;

        let mut db = app.db.acquire().await?;
        let write = crate::messages::require_writer(&mut db, &channel, &person(3)).await;
        assert_eq!(write.unwrap_err().code, "timed_out");
        assert!(!crate::messages::can_write(&mut db, &channel, &person(3), "text").await?);
        // DMs stay open, so they can still talk to a moderator.
        assert!(crate::messages::can_write(&mut db, &channel, &person(3), "dm").await?);
        drop(db);
        let invite = crate::members::invite(
            State(app.clone()),
            as_person(3),
            Ok(Json(serde_json::from_value(json!({
                "uses": 1,
                "ttl_seconds": 60,
                "space_key_ct": encode(&[0; rusttalk_core::keys::INVITE_BYTES]),
            }))?)),
        )
        .await;
        assert_eq!(invite.err().map(|e| e.code), Some("timed_out"));
        // Refused before LiveKit is even asked, which this test server doesn't have.
        let join = crate::voice::token(State(app.clone()), as_person(3), Path(room)).await;
        assert_eq!(join.err().map(|e| e.code), Some("timed_out"));
        let mut db = app.db.acquire().await?;
        let sources = crate::voice::publish_sources(&mut db, &channel, &person(3)).await?;
        assert!(sources.is_empty(), "a timed-out member shares nothing");
        drop(db);

        // Leaving and coming back doesn't end it.
        crate::members::leave(State(app.clone()), as_person(3)).await?;
        query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
            .bind(person(3))
            .execute(&app.db)
            .await?;
        let mut db = app.db.acquire().await?;
        assert_eq!(
            refuse(&mut db, &person(3)).await.unwrap_err().code,
            "timed_out"
        );
        Ok(())
    }
}
