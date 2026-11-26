//! Page pictures that stay while a page uses them. Message bodies are sealed, so the
//! editor who saves a page version lists the stored files it keeps; files the newest
//! version keeps never expire, and the others expire like any file again.

use std::collections::HashSet;

use rusttalk_core::auth::decode;
use sqlx::{SqliteConnection, query, query_scalar};

use crate::{ApiError, ApiResult, App, file_settings};

/// A message's pictures, each with its thumbnail.
pub const MAX_KEPT: usize = 2 * rusttalk_core::messages::MAX_ATTACHMENTS;

/// The files kept by the newest version of any page.
pub(crate) const KEPT: &str =
    "SELECT p.file_id FROM page_files p JOIN messages m ON m.id = p.message_id
     WHERE m.sequence = (SELECT MAX(sequence) FROM messages WHERE channel_id = p.channel_id)";

/// Whether a channel is a page (a text channel shown as its newest message).
pub(crate) async fn is_page(db: &mut SqliteConnection, channel: &str) -> ApiResult<bool> {
    Ok(
        query_scalar("SELECT EXISTS (SELECT 1 FROM channels WHERE id = ? AND page)")
            .bind(channel)
            .fetch_one(db)
            .await?,
    )
}

/// Check a page version's kept files (at most `MAX_KEPT` distinct IDs; any channel but a
/// page takes none) and return those that are unexpired files stored in this page channel.
/// Others are left out rather than refused: a page whose picture expired must still save.
pub(crate) async fn check(
    db: &mut SqliteConnection,
    channel: &str,
    page: bool,
    keep: Vec<String>,
    at: i64,
) -> ApiResult<Vec<String>> {
    let distinct: HashSet<&String> = keep.iter().collect();
    if !page || keep.len() > MAX_KEPT || distinct.len() != keep.len() {
        return Err(ApiError::bad_request("invalid_keep"));
    }
    for id in &keep {
        decode::<32>(id).map_err(|_| ApiError::bad_request("invalid_keep"))?;
    }
    let mut live = Vec::new();
    for id in keep {
        let stored: bool = query_scalar(
            "SELECT EXISTS (SELECT 1 FROM files WHERE id = ? AND channel_id = ? AND expires_at > ?)",
        )
        .bind(&id)
        .bind(channel)
        .bind(at)
        .fetch_one(&mut *db)
        .await?;
        if stored {
            live.push(id);
        }
    }
    Ok(live)
}

/// Remember the files a new page version keeps (already checked).
pub(crate) async fn record(
    db: &mut SqliteConnection,
    channel: &str,
    message: &str,
    keep: &[String],
) -> ApiResult<()> {
    for id in keep {
        query("INSERT INTO page_files (message_id, file_id, channel_id) VALUES (?, ?, ?)")
            .bind(message)
            .bind(id)
            .bind(channel)
            .execute(&mut *db)
            .await?;
    }
    Ok(())
}

/// After a page channel's newest version changed: keep the files it lists, and give
/// files no longer listed their normal expiry from upload again. Shared files stay kept.
pub(crate) async fn refresh(app: &App, db: &mut SqliteConnection, channel: &str) -> ApiResult<()> {
    let retention = file_settings::effective(&mut *db, &app.files)
        .await?
        .retention_secs;
    let now = crate::store::now()?;
    query(
        "DELETE FROM page_files WHERE channel_id = ?
             AND message_id NOT IN (SELECT id FROM messages WHERE channel_id = ?)",
    )
    .bind(channel)
    .bind(channel)
    .execute(&mut *db)
    .await?;
    const NEWEST: &str = "SELECT p.file_id FROM page_files p JOIN messages m ON m.id = p.message_id
         WHERE m.sequence = (SELECT MAX(sequence) FROM messages WHERE channel_id = ?)";
    query(&format!(
        "UPDATE files SET expires_at = created_at + ?
         WHERE channel_id = ? AND expires_at = ? AND id NOT IN (SELECT id FROM shared_files)
             AND id NOT IN ({NEWEST})"
    ))
    .bind(retention)
    .bind(channel)
    .bind(i64::MAX)
    .bind(channel)
    .execute(&mut *db)
    .await?;
    // An already expired file stays expired.
    query(&format!(
        "UPDATE files SET expires_at = ? WHERE channel_id = ? AND expires_at > ? AND id IN ({NEWEST})"
    ))
    .bind(i64::MAX)
    .bind(channel)
    .bind(now)
    .bind(channel)
    .execute(&mut *db)
    .await?;
    Ok(())
}

/// Stop keeping the files a person's page versions (only those from `device`, when given)
/// keep: they left, were removed, or that device was revoked.
pub(crate) async fn release_from(
    app: &App,
    db: &mut SqliteConnection,
    person: &[u8],
    device: Option<&[u8]>,
) -> ApiResult<()> {
    let channels: Vec<String> = query_scalar(
        // Walks the small page_files table and looks each version up by id, so the
        // messages table is never scanned while holding the writer.
        "DELETE FROM page_files WHERE EXISTS (SELECT 1 FROM messages m
             WHERE m.id = page_files.message_id AND m.sender_person = ?
                 AND (? IS NULL OR m.sender_device = ?))
         RETURNING channel_id",
    )
    .bind(person)
    .bind(device)
    .bind(device)
    .fetch_all(&mut *db)
    .await?;
    let channels: HashSet<String> = channels.into_iter().collect();
    for channel in &channels {
        refresh(app, db, channel).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, person};
    use anyhow::Result;
    use rusttalk_core::auth::encode;

    async fn expiry(app: &App, id: &str) -> Result<Option<i64>> {
        Ok(query_scalar("SELECT expires_at FROM files WHERE id = ?")
            .bind(id)
            .fetch_optional(&app.db)
            .await?)
    }

    async fn save(app: &App, version: &str, keep: &[&str]) -> Result<()> {
        let mut tx = crate::store::write(&app.db).await?;
        query(
            "INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
             VALUES (?, 'p', 1, X'01', ?, 0, 111, zeroblob(111))",
        )
        .bind(version)
        .bind(person(1))
        .execute(&mut *tx)
        .await?;
        let keep: Vec<String> = keep.iter().map(|id| id.to_string()).collect();
        record(&mut tx, "p", version, &keep).await?;
        refresh(app, &mut tx, "p").await?;
        tx.commit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn the_newest_page_version_keeps_its_files_until_it_drops_them() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        // Opaque rows seeded directly, so the test needs no signatures.
        sqlx::raw_sql(
            "INSERT INTO categories VALUES ('category', 0, X'00');
             INSERT INTO channels (id, kind, category_id, position, name_ct)
                 VALUES ('p', 'text', 'category', 0, X'00'), ('t', 'text', 'category', 0, X'00');
             UPDATE channels SET page = TRUE WHERE id = 'p';
             INSERT INTO key_scopes (id, kind, channel_id) SELECT id, 'channel', id FROM channels;
             INSERT INTO key_epochs (scope_id, epoch, commitment) SELECT id, 1, zeroblob(32) FROM key_scopes;",
        )
        .execute(&app.db)
        .await?;
        query("INSERT INTO message_senders VALUES (X'01', ?, X'03')")
            .bind(person(1))
            .execute(&app.db)
            .await?;
        let ids: Vec<String> = (1..=4u8).map(|byte| encode(&[byte; 32])).collect();
        let [one, two, other, expired] = [&ids[0], &ids[1], &ids[2], &ids[3]];
        let files = [
            (one, "p", 4_000_000_000i64),
            (two, "p", 4_000_000_000),
            (other, "t", 4_000_000_000),
            (expired, "p", 400),
        ];
        for (id, channel, expires_at) in files {
            query(
                "INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
                 VALUES (?, ?, ?, 16, 100, ?)",
            )
            .bind(id)
            .bind(channel)
            .bind(person(1))
            .bind(expires_at)
            .execute(&app.db)
            .await?;
        }
        let mut db = app.db.acquire().await?;
        let mut checked = async |page: bool, keep: &[&String]| {
            let keep: Vec<String> = keep.iter().map(|id| id.to_string()).collect();
            check(&mut db, "p", page, keep, 500)
                .await
                .map(|live| live.len())
                .map_err(|e| e.code)
        };
        assert_eq!(checked(false, &[one]).await, Err("invalid_keep"));
        assert_eq!(checked(true, &[one, one]).await, Err("invalid_keep"));
        assert_eq!(checked(true, &[one; 9]).await, Err("invalid_keep"));
        assert_eq!(checked(true, &[one, two]).await, Ok(2));
        // A gone picture or another channel's file is left out; the save still goes through.
        assert_eq!(checked(true, &[one, expired, other]).await, Ok(1));
        drop(db);

        save(&app, "v1", &[one, two]).await?;
        assert_eq!(expiry(&app, one).await?, Some(i64::MAX));
        assert_eq!(expiry(&app, two).await?, Some(i64::MAX));
        // The sweep passes over kept files, however old.
        crate::files::expire_batch(&app).await?;
        assert_eq!(expiry(&app, two).await?, Some(i64::MAX));

        // A newer version without the second picture: it expires from its upload time.
        save(&app, "v2", &[one]).await?;
        let retention = file_settings::effective(&mut *app.db.acquire().await?, &app.files)
            .await?
            .retention_secs;
        assert_eq!(expiry(&app, two).await?, Some(100 + retention));
        crate::files::expire_batch(&app).await?;
        assert_eq!(expiry(&app, two).await?, None);
        assert_eq!(expiry(&app, one).await?, Some(i64::MAX));

        // Deleting that version makes the first one newest again; its gone file stays gone.
        let mut tx = crate::store::write(&app.db).await?;
        query("DELETE FROM messages WHERE id = 'v2'")
            .execute(&mut *tx)
            .await?;
        refresh(&app, &mut tx, "p").await?;
        tx.commit().await?;
        assert_eq!(expiry(&app, one).await?, Some(i64::MAX));
        let rows: i64 = query_scalar("SELECT COUNT(*) FROM page_files WHERE message_id = 'v2'")
            .fetch_one(&app.db)
            .await?;
        assert_eq!(rows, 0);

        // An older app saves without a list: nothing stays kept.
        save(&app, "v3", &[]).await?;
        assert_eq!(expiry(&app, one).await?, Some(100 + retention));
        Ok(())
    }

    #[tokio::test]
    async fn a_leaving_editor_stops_keeping_files() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        sqlx::raw_sql(
            "INSERT INTO categories VALUES ('category', 0, X'00');
             INSERT INTO channels (id, kind, category_id, position, name_ct)
                 VALUES ('p', 'text', 'category', 0, X'00');
             UPDATE channels SET page = TRUE WHERE id = 'p';
             INSERT INTO key_scopes (id, kind, channel_id) SELECT id, 'channel', id FROM channels;
             INSERT INTO key_epochs (scope_id, epoch, commitment) SELECT id, 1, zeroblob(32) FROM key_scopes;",
        )
        .execute(&app.db)
        .await?;
        for byte in [1, 2] {
            query("INSERT INTO message_senders VALUES (?, ?, X'03')")
                .bind(vec![byte])
                .bind(person(byte))
                .execute(&app.db)
                .await?;
        }
        let id = encode(&[1; 32]);
        query(
            "INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
             VALUES (?, 'p', ?, 16, 100, 4000000000)",
        )
        .bind(&id)
        .bind(person(2))
        .execute(&app.db)
        .await?;
        save(&app, "v1", &[&id]).await?;
        assert_eq!(expiry(&app, &id).await?, Some(i64::MAX));
        // Its uploader also shared it, then leaves: the page still keeps it.
        query(
            "INSERT INTO shared_files (id, epoch, sender_device, sender_person, at, envelope)
             VALUES (?, 1, X'02', ?, 100, X'00')",
        )
        .bind(&id)
        .bind(person(2))
        .execute(&app.db)
        .await?;
        let mut tx = crate::store::write(&app.db).await?;
        crate::files::unshare_from(&app, &mut tx, &person(2), None).await?;
        tx.commit().await?;
        assert_eq!(expiry(&app, &id).await?, Some(i64::MAX));
        let mut tx = crate::store::write(&app.db).await?;
        // Another device of the editor changes nothing; theirs does.
        release_from(&app, &mut tx, &person(1), Some(&[2])).await?;
        tx.commit().await?;
        assert_eq!(expiry(&app, &id).await?, Some(i64::MAX));
        let mut tx = crate::store::write(&app.db).await?;
        release_from(&app, &mut tx, &person(1), None).await?;
        tx.commit().await?;
        assert_ne!(expiry(&app, &id).await?, Some(i64::MAX));
        Ok(())
    }

    #[tokio::test]
    async fn releasing_drops_only_that_persons_or_devices_versions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        sqlx::raw_sql(
            "INSERT INTO categories VALUES ('category', 0, X'00');
             INSERT INTO channels (id, kind, category_id, position, name_ct)
                 VALUES ('p', 'text', 'category', 0, X'00');
             UPDATE channels SET page = TRUE WHERE id = 'p';
             INSERT INTO key_scopes (id, kind, channel_id) SELECT id, 'channel', id FROM channels;
             INSERT INTO key_epochs (scope_id, epoch, commitment) SELECT id, 1, zeroblob(32) FROM key_scopes;",
        )
        .execute(&app.db)
        .await?;
        // Person 1 saved v1 from device 1 and v2 from device 2; person 2 saved v3 (newest).
        let versions = [("v1", 1u8, 1u8), ("v2", 2, 1), ("v3", 3, 2)];
        let mut tx = crate::store::write(&app.db).await?;
        for (n, (version, device, editor)) in versions.into_iter().enumerate() {
            query("INSERT INTO message_senders VALUES (?, ?, X'03')")
                .bind(vec![device])
                .bind(person(editor))
                .execute(&mut *tx)
                .await?;
            let file = encode(&[n as u8 + 1; 32]);
            query(
                "INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
                 VALUES (?, 'p', ?, 16, 100, 4000000000)",
            )
            .bind(&file)
            .bind(person(editor))
            .execute(&mut *tx)
            .await?;
            query(
                "INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
                 VALUES (?, 'p', 1, ?, ?, 0, 111, zeroblob(111))",
            )
            .bind(version)
            .bind(vec![device])
            .bind(person(editor))
            .execute(&mut *tx)
            .await?;
            record(&mut tx, "p", version, &[file]).await?;
        }
        refresh(&app, &mut tx, "p").await?;
        tx.commit().await?;
        let kept = async || -> Result<Vec<String>> {
            Ok(
                query_scalar("SELECT message_id FROM page_files ORDER BY message_id")
                    .fetch_all(&app.db)
                    .await?,
            )
        };
        let release = async |editor: u8, device: Option<u8>| -> Result<()> {
            let device = device.map(|byte| vec![byte]);
            let mut tx = crate::store::write(&app.db).await?;
            release_from(&app, &mut tx, &person(editor), device.as_deref()).await?;
            tx.commit().await?;
            Ok(())
        };
        assert_eq!(kept().await?, ["v1", "v2", "v3"]);
        // A device person 1 does not have, or another person's device, releases nothing.
        release(1, Some(3)).await?;
        assert_eq!(kept().await?, ["v1", "v2", "v3"]);
        // Revoking device 1 releases only what that device saved.
        release(1, Some(1)).await?;
        assert_eq!(kept().await?, ["v2", "v3"]);
        // Leaving releases the rest of person 1's versions, never person 2's.
        release(1, None).await?;
        assert_eq!(kept().await?, ["v3"]);
        assert_eq!(expiry(&app, &encode(&[3; 32])).await?, Some(i64::MAX));
        Ok(())
    }
}
