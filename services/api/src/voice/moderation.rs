//! Moderators muting people in voice, and removing or moving them out of a voice channel.
//!
//! A mute is server-wide and kept on the member, so it holds across channels and rejoins
//! until a moderator lifts it; leaving the server drops it with the member row. LiveKit
//! enforces it: tokens and `enforce` leave out the microphone source while it lasts.

use std::collections::BTreeSet;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, encode, voice_identity},
    keys::Event,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{SqliteConnection, query, query_scalar};

use super::{configured, enforce, require_voice_read};
use crate::{
    ApiError, ApiResult, App,
    auth::authenticate,
    keys, payload,
    roles::{self, Permission},
    store::audit,
};

pub async fn muted(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<bool> {
    Ok(query_scalar(
        "SELECT EXISTS (SELECT 1 FROM members WHERE person_id = ? AND voice_muted = 1)",
    )
    .bind(person)
    .fetch_one(db)
    .await?)
}

/// The actor holds `flag` and strictly outranks the target, as for a kick.
async fn require(
    db: &mut SqliteConnection,
    actor: &[u8],
    target: &[u8],
    flag: Permission,
) -> ApiResult<()> {
    let auth = roles::authority(db, actor).await?;
    auth.require(flag)?;
    auth.outrank(&roles::authority(db, target).await?)
}

pub async fn mute(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
) -> ApiResult<StatusCode> {
    set_muted(&app, &headers, &person, true).await
}

pub async fn unmute(
    State(app): State<App>,
    headers: HeaderMap,
    Path(person): Path<String>,
) -> ApiResult<StatusCode> {
    set_muted(&app, &headers, &person, false).await
}

async fn set_muted(
    app: &App,
    headers: &HeaderMap,
    person: &str,
    on: bool,
) -> ApiResult<StatusCode> {
    let person = decode::<32>(person)?;
    let (mut tx, actor) = authenticate(app, headers).await?;
    require(&mut tx, &actor.person, &person, Permission::MuteMembers).await?;
    let before: bool = query_scalar("SELECT voice_muted FROM members WHERE person_id = ?")
        .bind(person.as_slice())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::not_found("member_not_found"))?;
    if before == on {
        return Ok(StatusCode::NO_CONTENT);
    }
    query("UPDATE members SET voice_muted = ? WHERE person_id = ?")
        .bind(on)
        .bind(person.as_slice())
        .execute(&mut *tx)
        .await?;
    let action = if on { "voice_muted" } else { "voice_unmuted" };
    audit(
        &mut tx,
        &actor.person,
        action,
        Some(&person),
        None,
        actor.at,
    )
    .await?;
    tx.commit().await?;
    // Member lists show the mute. When it is lifted, the person's client rejoins its call
    // for a token with the microphone again.
    keys::changed(app, &person, None);
    if on {
        enforce(app, None);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn remove(
    State(app): State<App>,
    headers: HeaderMap,
    Path((channel, person)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    take_out(&app, &headers, channel, &person, None).await
}

#[derive(Deserialize)]
pub struct Move {
    to: String,
}

pub async fn move_to(
    State(app): State<App>,
    headers: HeaderMap,
    Path((channel, person)): Path<(String, String)>,
    body: Result<Json<Move>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let to = payload(body)?.to;
    take_out(&app, &headers, channel, &person, Some(to)).await
}

/// Disconnect a person's devices from a voice room, after telling them where to go if moved.
async fn take_out(
    app: &App,
    headers: &HeaderMap,
    channel: String,
    person: &str,
    to: Option<String>,
) -> ApiResult<StatusCode> {
    let person = decode::<32>(person)?;
    let (mut tx, actor) = authenticate(app, headers).await?;
    require_voice_read(&mut tx, &channel, &actor.person).await?;
    require(&mut tx, &actor.person, &person, Permission::MoveMembers).await?;
    if let Some(to) = &to {
        if *to == channel {
            return Err(ApiError::bad_request("same_voice_channel"));
        }
        require_voice_read(&mut tx, to, &actor.person).await?;
        // They must be allowed in, as when joining it themselves.
        require_voice_read(&mut tx, to, &person)
            .await
            .map_err(|_| ApiError::forbidden("voice_move_denied"))?;
    }
    let devices: Vec<Vec<u8>> =
        query_scalar("SELECT device_id FROM member_devices WHERE person_id = ?")
            .bind(person.as_slice())
            .fetch_all(&mut *tx)
            .await?;
    tx.commit().await?;

    let livekit = configured(app)?;
    let server = app.key.verifying_key();
    let identities: BTreeSet<_> = (devices.iter())
        .map(|device| voice_identity(server.as_bytes(), device))
        .collect();
    let present: Vec<_> = (livekit.list(&channel).await?.into_iter())
        .map(|participant| participant.identity)
        .filter(|identity| identities.contains(identity))
        .collect();
    if present.is_empty() {
        return Err(ApiError::not_found("not_in_voice"));
    }

    let (mut tx, actor) = authenticate(app, headers).await?;
    let (action, target) = match &to {
        Some(to) => ("voice_moved", to),
        None => ("voice_removed", &channel),
    };
    audit(
        &mut tx,
        &actor.person,
        action,
        Some(&person),
        Some(target),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    if let Some(to) = to {
        // Sent before the removal so their client switches rooms instead of reporting it.
        app.notify(Event::VoiceMoved {
            person: encode(&person),
            from: channel.clone(),
            to,
        });
    }
    for identity in present {
        let body = json!({ "room": channel, "identity": identity });
        livekit.moderate(&channel, "RemoveParticipant", body).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person, person};
    use livekit_protocol::TrackSource;

    /// Person 2 moderates; person 3 is a member; person 4 is an admin.
    async fn server(dir: &std::path::Path) -> anyhow::Result<App> {
        let app = app(dir, 4).await?;
        for (who, role) in [(2, "moderator"), (2, "member"), (3, "member"), (4, "admin")] {
            query("INSERT INTO member_roles (person_id, role_id) VALUES (?, ?)")
                .bind(person(who))
                .bind(role)
                .execute(&app.db)
                .await?;
        }
        query("INSERT INTO categories (id, position, name_ct) VALUES ('cat', 0, x'00')")
            .execute(&app.db)
            .await?;
        for (id, readers) in [(channel(1), "member"), (channel(2), "admin")] {
            query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, 'voice', 'cat', 0, x'00')")
                .bind(&id).execute(&app.db).await?;
            query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES (?, ?, 15)")
                .bind(&id)
                .bind(readers)
                .execute(&app.db)
                .await?;
        }
        Ok(app)
    }

    fn channel(byte: u8) -> String {
        encode(&[byte; 32])
    }

    #[tokio::test]
    async fn a_mute_follows_the_hierarchy_and_takes_the_microphone() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = server(dir.path()).await?;
        let set = |who: u8, target: u8, on: bool| {
            let target = encode(&person(target));
            let (app, headers) = (State(app.clone()), as_person(who));
            async move {
                match on {
                    true => mute(app, headers, Path(target)).await,
                    false => unmute(app, headers, Path(target)).await,
                }
            }
        };
        for (who, target, code) in [
            (3, 2, "permission_denied"),
            (2, 4, "role_hierarchy"),
            (2, 1, "role_hierarchy"),
            (2, 2, "role_hierarchy"),
            (4, 9, "member_not_found"),
        ] {
            let refused = set(who, target, true).await.unwrap_err();
            assert_eq!(refused.code, code, "{who} muting {target}");
        }
        let sources = || async {
            let mut db = app.db.acquire().await?;
            super::super::publish_sources(&mut db, &channel(1), &person(3)).await
        };
        assert!(sources().await?.contains(&TrackSource::Microphone));

        let mut events = app.changes.subscribe();
        assert_eq!(set(2, 3, true).await?, StatusCode::NO_CONTENT);
        assert!(events.try_recv().is_ok(), "member lists refresh");
        // Muting again changes nothing and logs nothing.
        assert_eq!(set(4, 3, true).await?, StatusCode::NO_CONTENT);
        assert!(events.try_recv().is_err());
        assert_eq!(
            sources().await?,
            [
                TrackSource::ScreenShareAudio,
                TrackSource::Camera,
                TrackSource::ScreenShare
            ]
        );
        let Json(members) = crate::metadata::members(State(app.clone()), as_person(1)).await?;
        let listed = serde_json::to_value(members)?;
        let flagged: Vec<_> = (listed.as_array().unwrap().iter())
            .filter(|member| member["voice_muted"] == true)
            .map(|member| member["person_id"].clone())
            .collect();
        assert_eq!(flagged, [encode(&person(3))]);

        assert_eq!(set(2, 3, false).await?, StatusCode::NO_CONTENT);
        assert!(sources().await?.contains(&TrackSource::Microphone));
        let audited: Vec<String> = query_scalar(
            "SELECT action FROM audit_log WHERE action LIKE 'voice_%' AND target_person_id = ? ORDER BY id",
        )
        .bind(person(3))
        .fetch_all(&app.db)
        .await?;
        assert_eq!(audited, ["voice_muted", "voice_unmuted"]);
        Ok(())
    }

    #[tokio::test]
    async fn removing_and_moving_check_permission_rank_and_access() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = server(dir.path()).await?;
        let take = |who: u8, target: u8, to: Option<String>| {
            let path = Path((channel(1), encode(&person(target))));
            let (app, headers) = (State(app.clone()), as_person(who));
            async move {
                match to {
                    None => remove(app, headers, path).await,
                    Some(to) => move_to(app, headers, path, Ok(Json(Move { to }))).await,
                }
            }
        };
        for (who, target, to, code) in [
            (3, 2, None, "permission_denied"),
            (2, 4, None, "role_hierarchy"),
            (2, 1, Some(channel(2)), "role_hierarchy"),
            // The moderator can't see the admin-only channel.
            (2, 3, Some(channel(2)), "channel_read_denied"),
            // The owner can, but the member may not join it.
            (1, 3, Some(channel(2)), "voice_move_denied"),
            (1, 3, Some(channel(1)), "same_voice_channel"),
            // Every check passed; only LiveKit is missing here.
            (2, 3, None, "voice_unavailable"),
            (1, 4, Some(channel(2)), "voice_unavailable"),
        ] {
            let refused = take(who, target, to.clone()).await.unwrap_err();
            assert_eq!(refused.code, code, "{who} taking {target} to {to:?}");
        }
        let audited: i64 = query_scalar("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&app.db)
            .await?;
        assert_eq!(audited, 0, "nothing logged before LiveKit confirms");
        Ok(())
    }
}
