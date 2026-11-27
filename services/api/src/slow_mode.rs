//! Slow mode: how long each member waits between messages in a channel.

use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, MutexGuard, PoisonError},
};

use rusttalk_core::identity::PersonId;
use sqlx::{SqliteConnection, query_scalar};

use crate::{
    ApiError, ApiResult,
    auth::Actor,
    roles::{self, Permission},
};

/// When each person may next post in each slow mode channel, in unix seconds. In memory:
/// a restart only lets people post once more a little early.
static NEXT_POST: LazyLock<Mutex<Waits>> = LazyLock::new(Mutex::default);

#[derive(Default)]
struct Waits {
    until: HashMap<(String, PersonId), i64>,
    pruned_at: i64,
}

fn waits() -> MutexGuard<'static, Waits> {
    // Nothing panics halfway through a change, so the map is consistent after a poisoning panic.
    NEXT_POST.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Refuse a new message in `channel` with 429 while its slow mode still has this person
/// waiting, and start their wait otherwise. A follow-up (an edit, vote, reaction or a
/// post's second part, as the app says) doesn't count, nor do people who may bypass it.
pub async fn check(
    db: &mut SqliteConnection,
    actor: &Actor,
    channel: &str,
    follow_up: bool,
) -> ApiResult<()> {
    if follow_up {
        return Ok(());
    }
    let secs: i64 = query_scalar("SELECT slow_mode_secs FROM channels WHERE id = ?")
        .bind(channel)
        .fetch_optional(&mut *db)
        .await?
        .unwrap_or(0);
    if secs <= 0
        || roles::authority(db, &actor.person)
            .await?
            .has(Permission::BypassSlowMode)
    {
        return Ok(());
    }
    wait(channel, actor.person, secs, actor.at)
}

fn wait(channel: &str, person: PersonId, secs: i64, now: i64) -> ApiResult<()> {
    let mut waits = waits();
    // Drop finished waits now and then, so the map only holds people still waiting.
    if now - waits.pruned_at >= 60 {
        waits.until.retain(|_, until| *until > now);
        waits.pruned_at = now;
    }
    let key = (channel.to_owned(), person);
    if let Some(&until) = waits.until.get(&key)
        && until > now
    {
        return Err(ApiError::too_many("slow_mode", until - now));
    }
    waits.until.insert(key, now + secs);
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use sqlx::query;

    use super::*;
    use crate::testing;

    fn actor(byte: u8, at: i64) -> Actor {
        Actor {
            person: [byte; 32],
            device: vec![byte],
            ip: "192.0.2.1".into(),
            at,
            session_id: vec![byte],
            expires_at: i64::MAX,
        }
    }

    #[tokio::test]
    async fn a_second_post_inside_the_window_waits_unless_bypassed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = testing::app(dir.path(), 3).await?;
        let mut db = app.db.acquire().await?;
        query("INSERT INTO categories (id, position, name_ct) VALUES ('slow-c', 0, x'00')")
            .execute(&mut *db)
            .await?;
        query("INSERT INTO channels (id, kind, category_id, position, name_ct, slow_mode_secs) VALUES ('slow-t', 'text', 'slow-c', 0, x'01', 30), ('slow-f', 'text', 'slow-c', 1, x'01', 0)")
            .execute(&mut *db)
            .await?;
        // Person 3 is a moderator, who bypasses slow mode by default.
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'moderator')")
            .bind(testing::person(3))
            .execute(&mut *db)
            .await?;

        check(&mut db, &actor(2, 1000), "slow-t", false).await?;
        let refused = check(&mut db, &actor(2, 1012), "slow-t", false)
            .await
            .unwrap_err();
        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!((refused.code, refused.retry_after), ("slow_mode", Some(18)));
        // Edits and reactions don't count, and other channels and people have their own.
        check(&mut db, &actor(2, 1012), "slow-t", true).await?;
        check(&mut db, &actor(2, 1012), "slow-f", false).await?;
        check(&mut db, &actor(2, 1012), "slow-f", false).await?;
        check(&mut db, &actor(4, 1012), "slow-t", false).await?;
        check(&mut db, &actor(2, 1030), "slow-t", false).await?;
        // The owner and a moderator post as often as they like.
        for person in [1, 3] {
            check(&mut db, &actor(person, 1000), "slow-t", false).await?;
            check(&mut db, &actor(person, 1001), "slow-t", false).await?;
        }
        Ok(())
    }

    #[test]
    fn a_refusal_says_when_to_try_again() {
        let response =
            axum::response::IntoResponse::into_response(ApiError::too_many("slow_mode", 12));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "12");
    }
}
