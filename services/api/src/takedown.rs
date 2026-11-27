//! Operator takedowns (#679): the owner removes messages and files for a legal reason, such
//! as a notice from the authorities, optionally bans who posted them, and the server keeps
//! a record of it. Bodies are sealed, so the owner's app names what to remove: a message
//! with its edits and the stored files (and thumbnails) it carries, or IDs pasted in.

use std::collections::BTreeSet;

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{decode, encode},
    keys::Event,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate, authenticate_read, require_owner},
    keys, members, messages, page_files, payload, roles,
    store::{self, WriteTx, audit, audit_details},
    voice,
};

/// Message and file IDs one takedown may name: a message, its edits and its files.
pub const MAX_IDS: usize = 64;
const MAX_REASON_CHARS: usize = 1000;
const MAX_NOTICE_CHARS: usize = 200;
/// The ID list and the longest reason and notice in UTF-8, with room for the JSON.
pub const BODY_LIMIT: usize = 16 * 1024;

#[derive(Deserialize)]
pub struct Request {
    /// Messages and stored files, base64url. Each is looked up as either; unknown ones are
    /// left out of the record.
    pub ids: Vec<String>,
    /// Why, in the owner's own words. Kept in plain text.
    pub reason: String,
    /// The notice's reference, such as a case number. Kept in plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// Also ban the one account that posted it all.
    #[serde(default)]
    pub ban: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Record {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// The account banned in the same step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banned: Option<String>,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Item {
    /// `message` or `file`.
    pub kind: String,
    pub id: String,
    pub channel: String,
    pub author: String,
    pub size: i64,
    /// SHA-256 of the ciphertext as hex; missing for a file already gone from disk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

pub async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Request>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<Record>)> {
    let request = payload(body)?;
    let (tx, actor) = authenticate(&app, &headers).await?;
    // Files are unlinked before the commit; a dropped request must not stop halfway.
    let record = tokio::spawn(async move { take_down(&app, tx, &actor, request).await })
        .await
        .map_err(|_| ApiError::internal("takedown_task_failed"))??;
    Ok((StatusCode::CREATED, Json(record)))
}

pub async fn list(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Vec<Record>>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    require_owner(&mut tx, &actor.person).await?;
    let ids: Vec<i64> = query_scalar("SELECT id FROM takedowns ORDER BY id DESC")
        .fetch_all(&mut *tx)
        .await?;
    let mut records = Vec::with_capacity(ids.len());
    for id in ids {
        records.push(stored(&mut tx, id).await?);
    }
    tx.commit().await?;
    Ok(Json(records))
}

type RecordRow = (i64, Vec<u8>, String, Option<String>, Option<Vec<u8>>);
type ItemRow = (String, String, String, Vec<u8>, i64, Option<Vec<u8>>);

async fn stored(db: &mut SqliteConnection, id: i64) -> ApiResult<Record> {
    let (at, actor, reason, notice, banned): RecordRow = query_as(
        "SELECT at, actor_person_id, reason, notice, banned_person_id FROM takedowns WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&mut *db)
    .await?;
    let items: Vec<ItemRow> = query_as(
        "SELECT kind, item_id, channel_id, author_person_id, size, sha256 FROM takedown_items
         WHERE takedown_id = ? ORDER BY kind DESC, item_id",
    )
    .bind(id)
    .fetch_all(db)
    .await?;
    Ok(Record {
        id,
        at,
        actor: encode(&actor),
        reason,
        notice,
        banned: banned.as_deref().map(encode),
        items: (items.into_iter())
            .map(|(kind, id, channel, author, size, sha256)| Item {
                kind,
                id,
                channel,
                author: encode(&author),
                size,
                sha256: sha256.as_deref().map(hex),
            })
            .collect(),
    })
}

/// What a takedown found to remove, before it is recorded.
struct Found {
    kind: &'static str,
    id: String,
    channel: String,
    author: Vec<u8>,
    size: i64,
    sha256: Option<[u8; 32]>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Trimmed, required text of at most `max` characters.
fn text(value: &str, max: usize, code: &'static str) -> ApiResult<String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max {
        return Err(ApiError::bad_request(code));
    }
    Ok(value.to_owned())
}

/// Remove what `request` names, everywhere this server keeps it, as the owner `actor`, and
/// record it. Called with the request's write transaction, also by other moderation tools
/// (the report queue); it commits and sends the changes itself.
pub async fn take_down(
    app: &App,
    mut tx: WriteTx,
    actor: &Actor,
    request: Request,
) -> ApiResult<Record> {
    require_owner(&mut tx, &actor.person).await?;
    let reason = text(&request.reason, MAX_REASON_CHARS, "invalid_takedown_reason")?;
    let notice = (request.notice.as_deref())
        .filter(|notice| !notice.trim().is_empty())
        .map(|notice| text(notice, MAX_NOTICE_CHARS, "invalid_takedown_notice"))
        .transpose()?;
    if request.ids.is_empty() || request.ids.len() > MAX_IDS {
        return Err(ApiError::bad_request("invalid_takedown"));
    }
    let mut ids = Vec::new();
    for id in request.ids {
        decode::<32>(&id)?;
        if !ids.contains(&id) {
            ids.push(id);
        }
    }

    // Messages first: a page version's kept files go with it.
    let mut items = Vec::new();
    let mut removed_messages = Vec::new();
    let mut file_ids = Vec::new();
    for id in ids {
        let row: Option<(String, Vec<u8>, Vec<u8>)> =
            query_as("SELECT channel_id, sender_person, envelope FROM messages WHERE id = ?")
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((channel, author, envelope)) = row else {
            file_ids.push(id);
            continue;
        };
        let kept: Vec<String> = query_scalar("SELECT file_id FROM page_files WHERE message_id = ?")
            .bind(&id)
            .fetch_all(&mut *tx)
            .await?;
        file_ids.extend(kept);
        items.push(Found {
            kind: "message",
            id: id.clone(),
            channel: channel.clone(),
            author,
            size: envelope.len() as i64,
            sha256: Some(Sha256::digest(&envelope).into()),
        });
        removed_messages.push((channel, id));
    }
    let mut removed_files = Vec::new();
    for id in file_ids {
        if removed_files.contains(&id) {
            continue;
        }
        let row: Option<(String, Vec<u8>, i64)> =
            query_as("SELECT channel_id, uploader, size FROM files WHERE id = ?")
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((channel, author, size)) = row else {
            continue;
        };
        items.push(Found {
            kind: "file",
            id: id.clone(),
            channel,
            author,
            size,
            sha256: app.files.digest(&id).await?,
        });
        removed_files.push(id);
    }
    if items.is_empty() {
        return Err(ApiError::not_found("takedown_not_found"));
    }
    let authors: BTreeSet<&[u8]> = items.iter().map(|item| item.author.as_slice()).collect();
    let single_author = match authors.len() {
        1 => authors.first().map(|author| author.to_vec()),
        _ => None,
    };
    let banned = match request.ban {
        true => match &single_author {
            Some(author) => Some(
                <[u8; 32]>::try_from(author.as_slice())
                    .map_err(|_| ApiError::internal("invalid_stored_sender"))?,
            ),
            None => {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "takedown_several_authors",
                ));
            }
        },
        false => None,
    };
    // Refused before anything is unlinked: files can't come back with a rollback.
    if let Some(person) = &banned {
        let auth = roles::authority(&mut tx, &actor.person).await?;
        auth.outrank(&roles::authority(&mut tx, person).await?)?;
    }

    // Deleted rows are overwritten with zeros, not only unlinked from the tree.
    query("PRAGMA secure_delete = ON").execute(&mut *tx).await?;
    for (channel, id) in &removed_messages {
        let removed = query("DELETE FROM messages WHERE id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        messages::uncount(&mut tx, channel, removed).await?;
        if page_files::is_page(&mut tx, channel).await? {
            page_files::refresh(app, &mut tx, channel).await?;
        }
    }
    // Its row goes with the shared-file listing and page references (ON DELETE CASCADE).
    for id in &removed_files {
        app.files.remove(&mut tx, id).await?;
    }
    query("PRAGMA secure_delete = OFF")
        .execute(&mut *tx)
        .await?;

    let closed = match &banned {
        Some(person) => ban(app, &mut tx, actor, person).await?,
        None => Vec::new(),
    };
    let id: i64 = query_scalar(
        "INSERT INTO takedowns (at, actor_person_id, reason, notice, banned_person_id)
         VALUES (?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(actor.at)
    .bind(actor.person.as_slice())
    .bind(&reason)
    .bind(&notice)
    .bind(banned.as_ref().map(|person| person.as_slice()))
    .fetch_one(&mut *tx)
    .await?;
    for item in &items {
        query(
            "INSERT INTO takedown_items
                 (takedown_id, kind, item_id, channel_id, author_person_id, size, sha256)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(item.kind)
        .bind(&item.id)
        .bind(&item.channel)
        .bind(&item.author)
        .bind(item.size)
        .bind(item.sha256.as_ref().map(|hash| hash.as_slice()))
        .execute(&mut *tx)
        .await?;
    }
    // The reason and notice stay out of the audit log, which more people may read.
    audit_details(
        &mut tx,
        &actor.person,
        "content_taken_down",
        single_author.as_deref(),
        Some(&id.to_string()),
        Some(json!({
            "messages": removed_messages.len(),
            "files": removed_files.len(),
            "banned": banned.is_some(),
        })),
        actor.at,
    )
    .await?;
    let record = stored(&mut tx, id).await?;
    tx.commit().await?;
    tracing::info!(
        id,
        messages = removed_messages.len(),
        files = removed_files.len(),
        banned = banned.is_some(),
        "content taken down"
    );
    for (channel, id) in removed_messages {
        app.notify(Event::MessageDeleted { channel, id });
    }
    if let Some(person) = banned {
        store::log_closed(closed, actor.at, "banned");
        keys::changed(app, &person, None);
        voice::enforce(app, None);
    }
    store::checkpoint(&app.db).await;
    Ok(record)
}

/// Ban `person` (already checked) as a takedown's last step, like a ban from the member
/// list; someone already banned stays so. Returns the sessions it closed.
async fn ban(
    app: &App,
    db: &mut SqliteConnection,
    actor: &Actor,
    person: &[u8; 32],
) -> ApiResult<Vec<i64>> {
    let rank = roles::authority(&mut *db, &actor.person).await?.rank();
    let added = query(
        "INSERT INTO bans (person_id, banned_at, banned_by, banned_by_rank) VALUES (?, ?, ?, ?)
         ON CONFLICT (person_id) DO NOTHING",
    )
    .bind(person.as_slice())
    .bind(actor.at)
    .bind(actor.person.as_slice())
    .bind(rank)
    .execute(&mut *db)
    .await?
    .rows_affected();
    let closed = members::remove_member(app, db, person).await?;
    if added > 0 {
        audit(
            db,
            &actor.person,
            "member_banned",
            Some(person),
            None,
            actor.at,
        )
        .await?;
    }
    Ok(closed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person, person};

    fn id(byte: u8) -> String {
        encode(&[byte; 32])
    }

    fn request(ids: &[String], ban: bool) -> Result<Json<Request>, JsonRejection> {
        Ok(Json(Request {
            ids: ids.to_vec(),
            reason: "  Notice from the police  ".into(),
            notice: Some("Case 2026-0042".into()),
            ban,
        }))
    }

    /// A text channel with a message of person 2 and one of person 3, two files of person
    /// 2 there, and a page whose version by person 2 keeps a third file, also shared.
    async fn seed(app: &App, files: &std::path::Path) -> anyhow::Result<()> {
        query(
            "INSERT INTO categories (id, position, name_ct) VALUES ('c', 0, x'00');
             INSERT INTO channels (id, kind, category_id, position, name_ct, message_count)
                 VALUES ('t', 'text', 'c', 0, x'00', 2);
             INSERT INTO channels (id, kind, category_id, position, name_ct, message_count, page)
                 VALUES ('p', 'text', 'c', 1, x'00', 1, TRUE);
             INSERT INTO key_scopes (id, kind, channel_id)
                 VALUES ('t', 'channel', 't'), ('p', 'channel', 'p');
             INSERT INTO key_epochs (scope_id, epoch, commitment)
                 VALUES ('t', 1, zeroblob(32)), ('p', 1, zeroblob(32));",
        )
        .execute(&app.db)
        .await?;
        query("INSERT INTO message_senders VALUES (X'02', ?, X'00'), (X'03', ?, X'00')")
            .bind(person(2))
            .bind(person(3))
            .execute(&app.db)
            .await?;
        for (message, channel, sender) in [(1u8, "t", 2u8), (2, "t", 3), (3, "p", 2)] {
            query(
                "INSERT INTO messages
                     (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
                 VALUES (?, ?, 1, ?, ?, 0, 111, randomblob(111))",
            )
            .bind(id(message))
            .bind(channel)
            .bind(vec![sender])
            .bind(person(sender))
            .execute(&app.db)
            .await?;
        }
        for (file, channel) in [(11u8, "t"), (12, "t"), (13, "p")] {
            query(
                "INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
                 VALUES (?, ?, ?, 20, 0, ?)",
            )
            .bind(id(file))
            .bind(channel)
            .bind(person(2))
            .bind(i64::MAX)
            .execute(&app.db)
            .await?;
            std::fs::write(files.join(id(file)), [file; 20])?;
        }
        query("INSERT INTO page_files (message_id, file_id, channel_id) VALUES (?, ?, 'p')")
            .bind(id(3))
            .bind(id(13))
            .execute(&app.db)
            .await?;
        query(
            "INSERT INTO shared_files (id, epoch, sender_device, sender_person, at, envelope)
             VALUES (?, 1, X'02', ?, 0, x'00')",
        )
        .bind(id(13))
        .bind(person(2))
        .execute(&app.db)
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn only_the_owner_takes_down_and_files_leave_the_disk() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 3).await?;
        let files = dir.path().join("files");
        seed(&app, &files).await?;
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'admin')")
            .bind(person(3))
            .execute(&app.db)
            .await?;
        let take = |who: u8, ids: &[String], ban: bool| {
            create(State(app.clone()), as_person(who), request(ids, ban))
        };
        let code = |result: ApiResult<(StatusCode, Json<Record>)>| result.unwrap_err().code;

        // Not even an admin may, and refusals change nothing.
        assert_eq!(code(take(3, &[id(1)], false).await), "owner_required");
        assert_eq!(code(take(1, &[], false).await), "invalid_takedown");
        assert_eq!(code(take(1, &[id(99)], false).await), "takedown_not_found");
        assert_eq!(
            code(take(1, &[id(1), id(2)], true).await),
            "takedown_several_authors"
        );
        let blank = Request {
            ids: vec![id(1)],
            reason: " ".into(),
            notice: None,
            ban: false,
        };
        let refused = create(State(app.clone()), as_person(1), Ok(Json(blank))).await;
        assert_eq!(code(refused), "invalid_takedown_reason");
        assert!(files.join(id(11)).exists());

        // A message with its two files and an ID the server doesn't know, and a page
        // version with the shared file it keeps.
        let mut events = app.changes.subscribe();
        let ids = [id(1), id(11), id(12), id(3), id(98)];
        let (status, Json(record)) = take(1, &ids, true).await?;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(record.reason, "Notice from the police");
        assert_eq!(record.notice.as_deref(), Some("Case 2026-0042"));
        assert_eq!(record.banned, Some(encode(&person(2))));
        let listed: Vec<(&str, &str)> = (record.items.iter())
            .map(|item| (item.kind.as_str(), item.id.as_str()))
            .collect();
        let mut expected = [
            ("message", id(1)),
            ("message", id(3)),
            ("file", id(11)),
            ("file", id(12)),
            ("file", id(13)),
        ];
        expected.sort_by(|a, b| b.0.cmp(a.0).then(a.1.cmp(&b.1)));
        let expected: Vec<(&str, &str)> = (expected.iter())
            .map(|(kind, id)| (*kind, id.as_str()))
            .collect();
        assert_eq!(listed, expected);
        let file = record.items.iter().find(|item| item.id == id(11)).unwrap();
        let digest = hex(&Sha256::digest([11; 20]));
        assert_eq!(file.sha256.as_deref(), Some(digest.as_str()));
        assert!(
            record
                .items
                .iter()
                .all(|item| item.author == encode(&person(2)))
        );

        // Gone from disk and every table; only person 3's message stays.
        for file in [11, 12, 13] {
            assert!(!files.join(id(file)).exists());
        }
        let left: Vec<String> = query_scalar("SELECT id FROM messages")
            .fetch_all(&app.db)
            .await?;
        assert_eq!(left, [id(2)]);
        for table in ["files", "shared_files", "page_files"] {
            let count: i64 = query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&app.db)
                .await?;
            assert_eq!(count, 0, "{table}");
        }
        let counts: Vec<i64> = query_scalar("SELECT message_count FROM channels ORDER BY id")
            .fetch_all(&app.db)
            .await?;
        assert_eq!(counts, [0, 1]);
        let banned: bool = query_scalar("SELECT EXISTS (SELECT 1 FROM bans WHERE person_id = ?)")
            .bind(person(2))
            .fetch_one(&app.db)
            .await?;
        assert!(banned);
        let audited: Vec<(String, Option<String>)> =
            query_as("SELECT action, details FROM audit_log ORDER BY id")
                .fetch_all(&app.db)
                .await?;
        let details = json!({"messages": 2, "files": 3, "banned": true}).to_string();
        assert_eq!(
            audited,
            [
                ("member_banned".into(), None),
                ("content_taken_down".into(), Some(details)),
            ]
        );
        let mut deleted = Vec::new();
        while let Ok(change) = events.try_recv() {
            if let crate::Change::Update(notice) = change
                && let rusttalk_core::presence::Update::Event(Event::MessageDeleted { id, .. }) =
                    &notice.update
            {
                deleted.push(id.clone());
            }
        }
        assert_eq!(deleted, [id(1), id(3)]);

        // The owner lists it; nobody else may.
        let Json(listed) = list(State(app.clone()), as_person(1)).await?;
        assert_eq!(listed, [record]);
        assert_eq!(
            list(State(app.clone()), as_person(3))
                .await
                .unwrap_err()
                .code,
            "owner_required"
        );
        Ok(())
    }
}
