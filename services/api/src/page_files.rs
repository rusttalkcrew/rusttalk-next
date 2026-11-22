//! Page pictures that stay while a page uses them. Documents are sealed, so the editor
//! who saves one lists the stored files it keeps; those never expire while it is the
//! page, and the others expire like any file again. A page from before documents keeps
//! what its newest message listed until the channel has a document.

use std::collections::HashSet;

use rusttalk_core::{auth::decode, pages::MAX_KEPT};
use sqlx::{SqliteConnection, query, query_scalar};

use crate::{ApiError, ApiResult, App, file_settings};

/// The files any page keeps.
pub(crate) const KEPT: &str = "SELECT file_id FROM page_document_files
     UNION ALL
     SELECT p.file_id FROM page_files p JOIN messages m ON m.id = p.message_id
     WHERE m.sequence = (SELECT MAX(sequence) FROM messages WHERE channel_id = p.channel_id)
       AND NOT EXISTS (SELECT 1 FROM page_documents d WHERE d.channel_id = p.channel_id)";

/// Check a page save's kept files (at most `MAX_KEPT` distinct IDs) and return those that
/// are unexpired files stored in this page channel.
/// Others are left out rather than refused: a page whose picture expired must still save.
pub(crate) async fn check(
    db: &mut SqliteConnection,
    channel: &str,
    keep: Vec<String>,
    at: i64,
) -> ApiResult<Vec<String>> {
    let distinct: HashSet<&String> = keep.iter().collect();
    if keep.len() > MAX_KEPT || distinct.len() != keep.len() {
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

/// Replace what the channel's document keeps (already checked).
pub(crate) async fn record(
    db: &mut SqliteConnection,
    channel: &str,
    keep: &[String],
) -> ApiResult<()> {
    query("DELETE FROM page_document_files WHERE channel_id = ?")
        .bind(channel)
        .execute(&mut *db)
        .await?;
    for id in keep {
        query("INSERT INTO page_document_files (channel_id, file_id) VALUES (?, ?)")
            .bind(channel)
            .bind(id)
            .execute(&mut *db)
            .await?;
    }
    Ok(())
}

/// After what a page channel keeps changed: keep the files it lists, and give
/// files no longer listed their normal expiry from upload again. Shared files stay kept.
pub(crate) async fn refresh(app: &App, db: &mut SqliteConnection, channel: &str) -> ApiResult<()> {
    let retention = file_settings::effective(&mut *db, app.files.ceilings)
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
    query(&format!(
        "UPDATE files SET expires_at = created_at + ?
         WHERE channel_id = ? AND expires_at = ? AND id NOT IN (SELECT id FROM shared_files)
             AND id NOT IN ({KEPT})"
    ))
    .bind(retention)
    .bind(channel)
    .bind(i64::MAX)
    .execute(&mut *db)
    .await?;
    // An already expired file stays expired.
    query(&format!(
        "UPDATE files SET expires_at = ? WHERE channel_id = ? AND expires_at > ? AND id IN ({KEPT})"
    ))
    .bind(i64::MAX)
    .bind(channel)
    .bind(now)
    .execute(&mut *db)
    .await?;
    Ok(())
}

/// Stop keeping the files of the pages a person saved last (only those from `device`,
/// when given): they left, were removed, or that device was revoked.
pub(crate) async fn release_from(
    app: &App,
    db: &mut SqliteConnection,
    person: &[u8],
    device: Option<&[u8]>,
) -> ApiResult<()> {
    let mut channels: Vec<String> = query_scalar(
        "DELETE FROM page_files WHERE message_id IN (SELECT id FROM messages
             WHERE sender_person = ? AND (? IS NULL OR sender_device = ?))
         RETURNING channel_id",
    )
    .bind(person)
    .bind(device)
    .bind(device)
    .fetch_all(&mut *db)
    .await?;
    let documents: Vec<String> = query_scalar(
        "DELETE FROM page_document_files WHERE channel_id IN (SELECT channel_id FROM page_documents
             WHERE sender_person = ? AND (? IS NULL OR sender_device = ?))
         RETURNING channel_id",
    )
    .bind(person)
    .bind(device)
    .bind(device)
    .fetch_all(&mut *db)
    .await?;
    channels.extend(documents);
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
        for id in keep {
            query("INSERT INTO page_files (message_id, file_id, channel_id) VALUES (?, ?, 'p')")
                .bind(version)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        refresh(app, &mut tx, "p").await?;
        tx.commit().await?;
        Ok(())
    }

    async fn document(app: &App, version: i64, keep: &[&str]) -> Result<()> {
        let mut tx = crate::store::write(&app.db).await?;
        query(
            "INSERT INTO page_documents (channel_id, version, epoch, sender_device, sender_person, at, size, envelope)
             VALUES ('p', ?, 1, X'01', ?, 0, 105, zeroblob(105))
             ON CONFLICT (channel_id) DO UPDATE SET version = excluded.version",
        )
        .bind(version)
        .bind(person(1))
        .execute(&mut *tx)
        .await?;
        let keep: Vec<String> = keep.iter().map(|id| id.to_string()).collect();
        record(&mut tx, "p", &keep).await?;
        refresh(app, &mut tx, "p").await?;
        tx.commit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn a_document_takes_over_from_the_message_based_page() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 1).await?;
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
        query("INSERT INTO message_senders VALUES (X'01', ?, X'03')")
            .bind(person(1))
            .execute(&app.db)
            .await?;
        let (old, banner, next) = (encode(&[1; 32]), encode(&[2; 32]), encode(&[3; 32]));
        for id in [&old, &banner] {
            query(
                "INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
                 VALUES (?, 'p', ?, 16, 100, 4000000000)",
            )
            .bind(id)
            .bind(person(1))
            .execute(&app.db)
            .await?;
        }
        save(&app, "v1", &[&old]).await?;
        assert_eq!(
            expiry(&app, &old).await?,
            Some(i64::MAX),
            "kept until there is a document"
        );

        document(&app, 1, &[&banner]).await?;
        assert_eq!(expiry(&app, &banner).await?, Some(i64::MAX));
        assert_ne!(expiry(&app, &old).await?, Some(i64::MAX));
        crate::files::expire_batch(&app).await?;
        assert_eq!(
            expiry(&app, &banner).await?,
            Some(i64::MAX),
            "the sweep passes over it"
        );

        document(&app, 2, &[]).await?;
        assert_ne!(expiry(&app, &banner).await?, Some(i64::MAX));

        // A fresh upload: what was dropped has expired and stays so.
        query(
            "INSERT INTO files (id, channel_id, uploader, size, created_at, expires_at)
             VALUES (?, 'p', ?, 16, 100, 4000000000)",
        )
        .bind(&next)
        .bind(person(1))
        .execute(&app.db)
        .await?;
        document(&app, 3, &[&next]).await?;
        let mut tx = crate::store::write(&app.db).await?;
        release_from(&app, &mut tx, &person(1), Some(&[2])).await?;
        tx.commit().await?;
        assert_eq!(
            expiry(&app, &next).await?,
            Some(i64::MAX),
            "another device changes nothing"
        );
        let mut tx = crate::store::write(&app.db).await?;
        release_from(&app, &mut tx, &person(1), None).await?;
        tx.commit().await?;
        assert_ne!(expiry(&app, &next).await?, Some(i64::MAX));
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
        let mut checked = async |keep: &[&String]| {
            let keep: Vec<String> = keep.iter().map(|id| id.to_string()).collect();
            check(&mut db, "p", keep, 500)
                .await
                .map(|live| live.len())
                .map_err(|e| e.code)
        };
        assert_eq!(checked(&[one, one]).await, Err("invalid_keep"));
        assert_eq!(checked(&[one; MAX_KEPT + 1]).await, Err("invalid_keep"));
        assert_eq!(checked(&[one, two]).await, Ok(2));
        // A gone picture or another channel's file is left out; the save still goes through.
        assert_eq!(checked(&[one, expired, other]).await, Ok(1));
        drop(db);

        save(&app, "v1", &[one, two]).await?;
        assert_eq!(expiry(&app, one).await?, Some(i64::MAX));
        assert_eq!(expiry(&app, two).await?, Some(i64::MAX));
        // The sweep passes over kept files, however old.
        crate::files::expire_batch(&app).await?;
        assert_eq!(expiry(&app, two).await?, Some(i64::MAX));

        // A newer version without the second picture: it expires from its upload time.
        save(&app, "v2", &[one]).await?;
        let retention = file_settings::effective(&mut *app.db.acquire().await?, app.files.ceilings)
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
}
