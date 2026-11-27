use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode_bytes, encode},
    keys::{Event, MemberProfile, Metadata, Part},
    presence::Presence,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    keys, payload,
    roles::{self, Permission},
    store::audit_details,
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
    system_messages_ct: Option<String>,
    first_text_channel: Option<String>,
    name_ct: Option<String>,
    description_ct: Option<String>,
    icon_hash: Option<String>,
}

pub async fn space(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Space>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    let (name, description, icon_hash, system_messages): (Ciphertext, Ciphertext, Option<String>, Ciphertext) =
        query_as("SELECT name_ct, description_ct, icon_hash, system_messages_ct FROM server_settings WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
    // Pick once across the server, then check access so a private first channel
    // neither leaks its ID nor redirects notices into another channel.
    let first_text_channel = query_scalar(
        "SELECT channel_id FROM channel_readers WHERE person_id = ? AND channel_id = (
            SELECT c.id FROM channels c JOIN categories g ON c.category_id = g.id
            WHERE c.kind = 'text' AND NOT c.page AND NOT c.forum ORDER BY g.position, g.id, c.position, c.id LIMIT 1
        )",
    )
    .bind(actor.person.as_slice())
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(Space {
        first_text_channel,
        system_messages_ct: encoded(system_messages),
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
    system_messages_ct: Option<Option<String>>,
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
        system_messages_ct,
        name_ct,
        description_ct,
        icon_ct,
    } = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageServer)?;
    // Which parts changed, never their (encrypted) contents.
    let changed: Vec<&str> = [
        ("name", name_ct.is_some()),
        ("description", description_ct.is_some()),
        ("icon", icon_ct.is_some()),
        ("system_messages", system_messages_ct.is_some()),
    ]
    .into_iter()
    .filter_map(|(part, set)| set.then_some(part))
    .collect();
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let icon = optional(Metadata::ServerIcon, epoch, icon_ct.clone().flatten())?;
    sqlx::query(
        "UPDATE server_settings SET
            system_messages_ct = CASE WHEN ? THEN ? ELSE system_messages_ct END,
            name_ct = CASE WHEN ? THEN ? ELSE name_ct END,
            description_ct = CASE WHEN ? THEN ? ELSE description_ct END,
            icon_ct = CASE WHEN ? THEN ? ELSE icon_ct END,
            icon_hash = CASE WHEN ? THEN ? ELSE icon_hash END
         WHERE id = 1",
    )
    .bind(system_messages_ct.is_some())
    .bind(optional(
        Metadata::SystemMessages,
        epoch,
        system_messages_ct.flatten(),
    )?)
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
    if !changed.is_empty() {
        audit_details(
            &mut tx,
            &actor.person,
            "server_updated",
            None,
            None,
            Some(json!({ "changed": changed })),
            actor.at,
        )
        .await?;
    }
    tx.commit().await?;
    app.notify(Event::ChannelChanged {
        part: Some(Part::Space),
    });
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Profile {
    #[serde(default)]
    avatar_if_unset: bool,
    #[serde(default, deserialize_with = "patch_field")]
    display_name_ct: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch_field")]
    avatar_ct: Option<Option<String>>,
    #[serde(default, deserialize_with = "patch_field")]
    bio_ct: Option<Option<String>>,
}

pub async fn update_profile(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Profile>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Profile {
        avatar_if_unset,
        display_name_ct,
        avatar_ct,
        bio_ct,
    } = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let epoch = keys::space_epoch(&mut tx, &app).await?;
    let avatar = optional(Metadata::Avatar, epoch, avatar_ct.clone().flatten())?;
    let updated = sqlx::query(
        "UPDATE members SET
            display_name_ct = CASE WHEN ? THEN ? ELSE display_name_ct END,
            avatar_ct = CASE WHEN ? THEN ? ELSE avatar_ct END,
            avatar_hash = CASE WHEN ? THEN ? ELSE avatar_hash END,
            avatar_initialized = CASE WHEN ? THEN 1 ELSE avatar_initialized END,
            bio_ct = CASE WHEN ? THEN ? ELSE bio_ct END
         WHERE person_id = ? AND (NOT ? OR (avatar_initialized = 0 AND avatar_ct IS NULL))",
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
    .bind(avatar_ct.is_some())
    .bind(bio_ct.is_some())
    .bind(optional(Metadata::Bio, epoch, bio_ct.flatten())?)
    .bind(actor.person.as_slice())
    .bind(avatar_if_unset)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(StatusCode::PRECONDITION_FAILED);
    }
    let profile = profile(&mut tx, &actor.person).await?;
    tx.commit().await?;
    keys::profile_changed(&app, &actor.person, profile);
    Ok(StatusCode::NO_CONTENT)
}

/// A member's profile as the member list shows it, for a `member_changed` notice.
pub(crate) async fn profile(
    db: &mut SqliteConnection,
    person: &[u8],
) -> ApiResult<Option<MemberProfile>> {
    type ProfileRow = (
        Ciphertext,
        Option<String>,
        Ciphertext,
        bool,
        bool,
        bool,
        Option<i64>,
    );
    let row: Option<ProfileRow> = query_as(
        "SELECT display_name_ct, avatar_hash, bio_ct, hide_offline, avatar_initialized, voice_muted,
                (SELECT ends_at FROM timeouts t WHERE t.person_id = m.person_id AND ends_at > ?)
         FROM members m WHERE person_id = ?",
    )
    .bind(crate::store::now()?)
    .bind(person)
    .fetch_optional(db)
    .await?;
    Ok(row.map(
        |(
            display_name,
            avatar_hash,
            bio,
            hide_offline,
            avatar_initialized,
            voice_muted,
            timeout,
        )| {
            MemberProfile {
                display_name_ct: encoded(display_name),
                avatar_hash,
                bio_ct: encoded(bio),
                hide_offline,
                avatar_initialized,
                voice_muted,
                timeout_until: timeout,
            }
        },
    ))
}

#[derive(Serialize)]
pub struct Member {
    person_id: String,
    presence: Presence,
    display_name_ct: Option<String>,
    avatar_hash: Option<String>,
    avatar_initialized: bool,
    /// Always present (null without a bio), so clients can tell this server stores bios.
    bio_ct: Option<String>,
    role_ids: Vec<String>,
    /// Clients leave this member out of member lists while they are offline.
    hide_offline: bool,
    /// A moderator muted them in every voice channel.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    voice_muted: bool,
    /// A moderator put them in a timeout that ends then, in unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    timeout_until: Option<i64>,
}

/// Person, display name, avatar hash, bio, `hide_offline`, `voice_muted`, `avatar_initialized`
/// and a running timeout's end.
type MemberRow = (
    Vec<u8>,
    Ciphertext,
    Option<String>,
    Ciphertext,
    bool,
    bool,
    bool,
    Option<i64>,
);

pub async fn members(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Vec<Member>>> {
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let rows: Vec<MemberRow> = query_as(
        "SELECT person_id, display_name_ct, avatar_hash, bio_ct, hide_offline, voice_muted, avatar_initialized,
                (SELECT ends_at FROM timeouts t WHERE t.person_id = m.person_id AND ends_at > ?)
         FROM members m
         ORDER BY person_id",
    )
    .bind(crate::store::now()?)
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
    let knows_dnd = crate::presence::knows_dnd(&headers);
    Ok(Json(
        rows.into_iter()
            .map(
                |(
                    person,
                    display_name,
                    avatar_hash,
                    bio,
                    hide_offline,
                    voice_muted,
                    avatar_initialized,
                    timeout_until,
                )| {
                    let person_id = encode(&person);
                    Member {
                        presence: crate::presence::presence_for(
                            presence.get(&person_id).copied().unwrap_or_default(),
                            knows_dnd,
                        ),
                        person_id,
                        display_name_ct: encoded(display_name),
                        avatar_hash,
                        avatar_initialized,
                        bio_ct: encoded(bio),
                        role_ids: roles.remove(&person).unwrap_or_default(),
                        hide_offline,
                        voice_muted,
                        timeout_until,
                    }
                },
            )
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_avatar_is_atomic_preserves_clears_and_other_members() -> anyhow::Result<()> {
        use crate::testing::{app, as_person, person};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let server = keys::space(&app).id;
        let mut conn = app.db.acquire().await?;
        keys::init_space(&mut conn, &server).await?;
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(encode(&server))
            .execute(&mut *conn)
            .await?;
        drop(conn);
        let key = rusttalk_core::keys::random_key()?;
        let ct =
            encode(&Metadata::Avatar.seal(&server, &person(2), 1, &key, b"synthetic image")?);
        let write = |body| {
            update_profile(
                State(app.clone()),
                as_person(2),
                Ok(Json(serde_json::from_value(body).unwrap())),
            )
        };
        let fill = json!({"avatar_ct": ct, "avatar_if_unset": true});
        let mut changes = app.changes.subscribe();
        assert_eq!(write(fill.clone()).await?, StatusCode::NO_CONTENT);
        // Peers get the new profile inline, so they need no reload.
        let Ok(crate::Change::Update(notice)) = changes.try_recv() else {
            panic!("expected a profile notice");
        };
        let rusttalk_core::presence::Update::Event(Event::MemberChanged {
            person: changed,
            profile: Some(profile),
            ..
        }) = &notice.update
        else {
            panic!("expected a profile notice");
        };
        assert_eq!(*changed, encode(&person(2)));
        assert!(profile.avatar_initialized && profile.avatar_hash.is_some());
        assert_eq!(write(fill.clone()).await?, StatusCode::PRECONDITION_FAILED);
        write(json!({"avatar_ct": null})).await?;
        assert_eq!(write(fill).await?, StatusCode::PRECONDITION_FAILED);
        let roster = members(State(app.clone()), as_person(1)).await?.0;
        let cleared = roster
            .iter()
            .find(|m| m.person_id == encode(&person(2)))
            .unwrap();
        assert!(cleared.avatar_initialized && cleared.avatar_hash.is_none());
        assert!(
            !roster
                .iter()
                .find(|m| m.person_id == encode(&person(1)))
                .unwrap()
                .avatar_initialized
        );
        write(json!({"avatar_ct": ct})).await?;
        assert_eq!(
            avatar(State(app), as_person(1), Path(encode(&person(2))))
                .await?
                .0["avatar_ct"],
            ct
        );
        Ok(())
    }

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
    async fn system_messages_respect_permissions_order_and_partial_updates() -> anyhow::Result<()> {
        use crate::testing::{app, as_person};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        query("INSERT INTO categories (id, position, name_ct) VALUES ('g', 0, x'00')")
            .execute(&app.db)
            .await?;
        for (id, position, page) in [("page", 0, true), ("first", 1, false), ("next", 2, false)] {
            query("INSERT INTO channels (id, category_id, position, kind, page, name_ct) VALUES (?, 'g', ?, 'text', ?, x'00')")
                .bind(id).bind(position).bind(page).execute(&app.db).await?;
        }
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'member')")
            .bind(crate::testing::person(2))
            .execute(&app.db)
            .await?;
        query("INSERT INTO channel_permissions (channel_id, role_id, flags) VALUES ('next', 'member', 1)")
            .execute(&app.db).await?;
        let read = |who| space(State(app.clone()), as_person(who));
        assert_eq!(
            read(1).await?.0.first_text_channel.as_deref(),
            Some("first")
        );
        assert!(read(2).await?.0.first_text_channel.is_none());
        let server = keys::space(&app).id;
        let mut conn = app.db.acquire().await?;
        keys::init_space(&mut conn, &server).await?;
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(encode(&server))
            .execute(&mut *conn)
            .await?;
        drop(conn);
        let write = |who, body| {
            update_space(
                State(app.clone()),
                as_person(who),
                Ok(Json(serde_json::from_value(body).unwrap())),
            )
        };
        let key = rusttalk_core::keys::random_key()?;
        for choice in [&[1; 32][..], &[]] {
            let ct = encode(&Metadata::SystemMessages.seal(&server, &server, 1, &key, choice)?);
            let patch = json!({"system_messages_ct": ct});
            assert_eq!(
                write(2, patch.clone()).await.unwrap_err().status,
                StatusCode::FORBIDDEN
            );
            write(1, patch).await?;
            write(1, json!({"description_ct": null})).await?;
            assert_eq!(read(2).await?.0.system_messages_ct, Some(ct));
        }
        write(1, json!({"system_messages_ct": null})).await?;
        assert!(read(1).await?.0.system_messages_ct.is_none());
        // Each change is audited by which part changed, never its contents.
        write(1, json!({})).await?;
        let audited: Vec<String> = query_scalar(
            "SELECT details FROM audit_log WHERE action = 'server_updated' ORDER BY id",
        )
        .fetch_all(&app.db)
        .await?;
        let parts = |part: &str| json!({ "changed": [part] }).to_string();
        let system = parts("system_messages");
        let description = parts("description");
        assert_eq!(
            audited,
            [&system, &description, &system, &description, &system].map(String::as_str)
        );
        Ok(())
    }

    #[tokio::test]
    async fn members_set_their_own_encrypted_bio_within_the_metadata_cap() -> anyhow::Result<()> {
        use crate::testing::{app, as_person};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let server = keys::space(&app).id;
        let mut conn = app.db.acquire().await?;
        keys::init_space(&mut conn, &server).await?;
        query("INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES (?, 1, zeroblob(32))")
            .bind(encode(&server))
            .execute(&mut *conn)
            .await?;
        drop(conn);
        let key = rusttalk_core::keys::random_key()?;
        let me = crate::testing::person(2);
        let sealed = |clear: &[u8]| {
            Metadata::Bio
                .seal(&server, &me, 1, &key, clear)
                .map(|ct| encode(&ct))
        };
        let set = |headers, body: Value| {
            update_profile(
                State(app.clone()),
                headers,
                Ok(Json(serde_json::from_value(body).unwrap())),
            )
        };
        let bio_of = |row: usize| {
            let app = app.clone();
            async move {
                let Json(rows) = members(State(app), as_person(1)).await.unwrap();
                serde_json::to_value(&rows[row]).unwrap()["bio_ct"].clone()
            }
        };
        // Present but null before anyone writes one, so clients know bios are stored.
        assert_eq!(bio_of(1).await, Value::Null);

        let bio = sealed("Hi :wave:\nsecond line".as_bytes())?;
        let status = set(as_person(2), json!({ "bio_ct": bio })).await?;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(bio_of(1).await, json!(bio));
        // Only the caller's own row changes, and other fields stay.
        assert_eq!(bio_of(0).await, Value::Null);
        set(as_person(2), json!({ "display_name_ct": sealed(b"x")? })).await?;
        assert_eq!(bio_of(1).await, json!(bio));

        // The plaintext cap is the metadata cap (1024 bytes); the server sees only ciphertext.
        let fits = sealed(&[b'a'; rusttalk_core::keys::SMALL_METADATA])?;
        assert_eq!(
            set(as_person(2), json!({ "bio_ct": fits })).await?,
            StatusCode::NO_CONTENT
        );
        let mut too_long = Metadata::Bio.seal(&server, &me, 1, &key, b"a")?;
        too_long.resize(rusttalk_core::keys::SMALL_METADATA + 100, 0);
        let error = set(as_person(2), json!({ "bio_ct": encode(&too_long) }))
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_metadata");

        // No session, no write.
        let error = set(axum::http::HeaderMap::new(), json!({ "bio_ct": bio }))
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);

        // Null clears it.
        set(as_person(2), json!({ "bio_ct": null })).await?;
        assert_eq!(bio_of(1).await, Value::Null);
        Ok(())
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
        let _socket = crate::presence::open(&app, person, "d".into(), Status::Online);
        assert_eq!(listed().await, (json!(true), json!("online")));

        hide(false).await?;
        assert_eq!(listed().await.0, json!(false));
        Ok(())
    }

    #[tokio::test]
    async fn older_apps_read_do_not_disturb_as_away() -> anyhow::Result<()> {
        use crate::testing::{app, as_person};
        use rusttalk_core::presence::{FEATURES, FEATURES_HEADER, Status};
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let newer = |byte| {
            let mut headers = as_person(byte);
            headers.insert(FEATURES_HEADER, FEATURES.parse().unwrap());
            headers
        };
        let pick = serde_json::from_value(json!({"status": "dnd", "picked": true}))?;
        crate::presence::set(State(app.clone()), newer(2), Ok(Json(pick))).await?;
        let person = encode(&crate::testing::person(2));
        let _socket = crate::presence::open(&app, person, "d".into(), Status::Dnd);
        let listed = |headers| async {
            let Json(rows) = members(State(app.clone()), headers).await.unwrap();
            serde_json::to_value(&rows[1]).unwrap()["presence"].clone()
        };
        assert_eq!(listed(as_person(1)).await, "away");
        assert_eq!(listed(newer(1)).await, "dnd");
        let me = async |headers| {
            let Json(me) = crate::members::me(State(app.clone()), headers)
                .await
                .unwrap();
            serde_json::to_value(me).unwrap()["status"].clone()
        };
        assert_eq!(me(as_person(2)).await, "away");
        assert_eq!(me(newer(2)).await, "dnd");
        Ok(())
    }
}
