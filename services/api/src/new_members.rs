//! New member rules: for a while after joining, a member with only the default role can't
//! upload files, start DMs or create invites. Mentions of everyone and roles are encrypted,
//! so apps hold those back themselves (see `docs/protocol.md`).
//!
//! TODO(#659): once per-member rate limits exist, give members for whom `until` returns
//! `Some` the Strict preset; that is the hook.

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::HeaderMap,
};
use rusttalk_core::auth::NewMemberRules;
use sqlx::{SqliteConnection, query, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate, authenticate_read},
    payload,
    roles::{self, Permission},
    store::audit_details,
};

async fn period(db: &mut SqliteConnection) -> ApiResult<i64> {
    Ok(
        query_scalar("SELECT new_member_secs FROM server_settings WHERE id = 1")
            .fetch_one(db)
            .await?,
    )
}

/// When the rules stop applying to `person`, while they still do at `now`. The owner, people
/// with any role but the default and people who may skip the rules are never restricted.
pub async fn until(db: &mut SqliteConnection, person: &[u8], now: i64) -> ApiResult<Option<i64>> {
    let secs = period(db).await?;
    if secs <= 0 {
        return Ok(None);
    }
    let joined_at: Option<i64> = query_scalar(
        "SELECT m.joined_at FROM members m, server_settings s
         WHERE m.person_id = ? AND s.id = 1 AND s.owner_person_id IS NOT m.person_id
           AND NOT EXISTS (SELECT 1 FROM member_roles r
                           WHERE r.person_id = m.person_id AND r.role_id <> s.default_role_id)",
    )
    .bind(person)
    .fetch_optional(&mut *db)
    .await?;
    let Some(end) = joined_at
        .map(|at| at.saturating_add(secs))
        .filter(|end| *end > now)
    else {
        return Ok(None);
    };
    if roles::authority(db, person)
        .await?
        .has(Permission::SkipNewMemberRules)
    {
        return Ok(None);
    }
    Ok(Some(end))
}

/// Refuse with `403 new_member_rules` while the rules apply to the caller.
pub async fn check(db: &mut SqliteConnection, actor: &Actor) -> ApiResult<()> {
    if until(db, &actor.person, actor.at).await?.is_some() {
        return Err(ApiError::forbidden("new_member_rules"));
    }
    Ok(())
}

pub async fn get(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<NewMemberRules>> {
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let secs = period(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(NewMemberRules { secs }))
}

pub async fn set(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<NewMemberRules>, JsonRejection>,
) -> ApiResult<Json<NewMemberRules>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageServer)?;
    let secs = payload(body)?.secs;
    if !(0..=NewMemberRules::MAX_SECS).contains(&secs) {
        return Err(ApiError::bad_request("invalid_new_member_rules"));
    }
    let before = period(&mut tx).await?;
    if before != secs {
        query("UPDATE server_settings SET new_member_secs = ? WHERE id = 1")
            .bind(secs)
            .execute(&mut *tx)
            .await?;
        audit_details(
            &mut tx,
            &actor.person,
            "new_member_rules_changed",
            None,
            None,
            Some(serde_json::json!({ "before": before, "after": secs })),
            actor.at,
        )
        .await?;
    }
    tx.commit().await?;
    if before != secs {
        // Members reload `/v1/me`, which says until when the rules apply to them.
        app.notify(rusttalk_core::keys::Event::ChannelChanged { part: None });
    }
    Ok(Json(NewMemberRules { secs }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person, person};

    #[tokio::test]
    async fn rules_apply_to_new_default_role_members_until_their_period_ends() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 5).await?;
        // 2 and 3 have only the default role, 4 also a custom one, 5 is a moderator.
        for (who, role) in [
            (2, "member"),
            (3, "member"),
            (4, "member"),
            (5, "moderator"),
        ] {
            query("INSERT INTO member_roles (person_id, role_id) VALUES (?, ?)")
                .bind(person(who))
                .bind(role)
                .execute(&app.db)
                .await?;
        }
        query("INSERT INTO roles (id, kind, rank, permissions) VALUES ('friends', 'custom', 1, 0)")
            .execute(&app.db)
            .await?;
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'friends')")
            .bind(person(4))
            .execute(&app.db)
            .await?;
        query("UPDATE members SET joined_at = 1000")
            .execute(&app.db)
            .await?;
        let mut db = app.db.acquire().await?;
        // Off by default.
        assert_eq!(until(&mut db, &person(2), 1001).await?, None);

        // Members may read the setting; only Manage Server may change it, within 30 days.
        let save = |who: u8, secs: i64| {
            set(
                State(app.clone()),
                as_person(who),
                Ok(Json(NewMemberRules { secs })),
            )
        };
        assert_eq!(save(2, 600).await.unwrap_err().code, "permission_denied");
        let too_long = save(1, NewMemberRules::MAX_SECS + 1).await.unwrap_err();
        assert_eq!(too_long.code, "invalid_new_member_rules");
        let _ = save(1, 600).await?;
        let _ = save(1, 600).await?;
        let Json(read) = get(State(app.clone()), as_person(2)).await?;
        assert_eq!(read.secs, 600);

        assert_eq!(until(&mut db, &person(2), 1001).await?, Some(1600));
        assert_eq!(until(&mut db, &person(2), 1600).await?, None);
        for exempt in [1, 4, 5] {
            assert_eq!(until(&mut db, &person(exempt), 1001).await?, None);
        }
        // The new permission on the default role lets everyone skip them.
        query("UPDATE roles SET permissions = permissions | ? WHERE id = 'member'")
            .bind(Permission::SkipNewMemberRules as i64)
            .execute(&mut *db)
            .await?;
        assert_eq!(until(&mut db, &person(3), 1001).await?, None);

        // One audit entry for the one real change.
        let details: Vec<String> =
            query_scalar("SELECT details FROM audit_log WHERE action = 'new_member_rules_changed'")
                .fetch_all(&mut *db)
                .await?;
        let expected = serde_json::json!({ "before": 0, "after": 600 }).to_string();
        assert_eq!(details, [expected]);
        Ok(())
    }
}
