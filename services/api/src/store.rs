use std::{
    fs,
    io::{self, Read, Write},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use rusttalk_core::identity::random_bytes;
use sha2::{Digest, Sha256};
use sqlx::{
    Sqlite, SqliteConnection, SqlitePool, Transaction,
    migrate::Migrate,
    query, query_scalar,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use zeroize::Zeroizing;

use crate::{ApiError, ApiResult};

pub async fn open(dir: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(dir.join("rusttalk.sqlite3"))
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    tx.ensure_migrations_table().await?;
    query("CREATE TEMP TABLE upgrading_to_calendar AS SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = 34)")
        .execute(&mut *tx).await?;
    let migrations = sqlx::migrate!();
    // Forum previews used 32, now occupied by system messages. Match only that checksum.
    let forum = migrations
        .iter()
        .find(|m| m.version == 36)
        .expect("forum migration");
    query(
        "UPDATE _sqlx_migrations SET version = 36 WHERE version = 32 AND success AND checksum = ?",
    )
    .bind(forum.checksum.as_ref())
    .execute(&mut *tx)
    .await?;
    // Only this exact preview migration moved. SQLx still validates every other checksum.
    let preview = sha2::Sha384::digest(include_bytes!("../preview/0035_calendar_delegation.sql"));
    query("DELETE FROM _sqlx_migrations WHERE version = 35 AND success AND checksum = ?")
        .bind(&preview[..])
        .execute(&mut *tx)
        .await?;
    migrations.run(&mut *tx).await?;
    query("DROP TABLE IF EXISTS temp.upgrading_to_calendar")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    crate::metadata::hash_pictures(&pool).await?;
    Ok(pool)
}

pub fn server_key(dir: &Path) -> Result<(SigningKey, bool)> {
    let path = dir.join("server.key");
    let mut seed = Zeroizing::new([0; 32]);
    let read = |seed: &mut [u8; 32]| {
        let mut file = fs::File::open(&path)?;
        file.read_exact(seed)?;
        if file.read(&mut [0; 1])? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid server key length",
            ));
        }
        Ok(())
    };
    match read(&mut seed) {
        Ok(()) => return Ok((SigningKey::from_bytes(&seed), false)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("read server key"),
    }
    random_bytes(seed.as_mut())?;
    let mut file =
        tempfile::NamedTempFile::new_in(dir).context("create private server key file")?;
    file.write_all(seed.as_ref())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&path) {
        Ok(_) => {
            fs::File::open(dir)?.sync_all()?;
            Ok((SigningKey::from_bytes(&seed), true))
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            read(&mut seed).context("read installed server key")?;
            Ok((SigningKey::from_bytes(&seed), false))
        }
        Err(error) => Err(error.error).context("install server key file"),
    }
}

pub fn now() -> ApiResult<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| i64::try_from(time.as_secs()).ok())
        .ok_or(ApiError::internal("clock_error"))
}

pub fn random<const N: usize>() -> ApiResult<[u8; N]> {
    let mut bytes = [0; N];
    random_bytes(&mut bytes).map_err(|_| ApiError::internal("randomness_error"))?;
    Ok(bytes)
}

pub fn hash(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}

/// One writer at a time in this process. SQLite's busy handler gives up after the busy timeout
/// and is not fair, so writers queue here instead, without holding a pooled connection.
static WRITER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

type Turn = tokio::sync::MutexGuard<'static, ()>;

/// A write transaction that holds this process's writer turn until SQLite has really ended it.
/// SQLite keeps running a statement or commit after its request is cancelled, so the turn
/// passes only once the commit or rollback has completed, never when the future is dropped.
pub struct WriteTx(Option<(Transaction<'static, Sqlite>, Turn)>);

impl WriteTx {
    pub async fn commit(mut self) -> Result<(), sqlx::Error> {
        let (tx, turn) = self.0.take().expect("open write transaction");
        // The task owns the turn, so cancelling this request cannot pass it on mid-commit.
        tokio::spawn(async move {
            let committed = tx.commit().await;
            drop(turn);
            committed
        })
        .await
        .map_err(|_| sqlx::Error::WorkerCrashed)?
    }
}

impl Drop for WriteTx {
    fn drop(&mut self) {
        let Some((tx, turn)) = self.0.take() else {
            return;
        };
        // Awaiting the rollback also waits for a statement a cancelled request left running.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = tx.rollback().await;
                drop(turn);
            });
        }
    }
}

impl std::ops::Deref for WriteTx {
    type Target = SqliteConnection;
    fn deref(&self) -> &SqliteConnection {
        &self.0.as_ref().expect("open write transaction").0
    }
}

impl std::ops::DerefMut for WriteTx {
    fn deref_mut(&mut self) -> &mut SqliteConnection {
        &mut self.0.as_mut().expect("open write transaction").0
    }
}

/// Start a transaction that may write. Read-only work uses a deferred `begin()` instead: in WAL
/// mode it reads a snapshot and never waits for a writer.
pub async fn write(pool: &SqlitePool) -> ApiResult<WriteTx> {
    begin_write(pool, WRITER.lock().await).await
}

/// Like [`write`], but only if the writer turn is free right now; `None` if it would queue.
pub async fn try_write(pool: &SqlitePool) -> ApiResult<Option<WriteTx>> {
    match WRITER.try_lock() {
        Ok(turn) => Ok(Some(begin_write(pool, turn).await?)),
        Err(_) => Ok(None),
    }
}

async fn begin_write(pool: &SqlitePool, turn: Turn) -> ApiResult<WriteTx> {
    // IMMEDIATE takes the write lock up front, so it serializes authorization and mutation with
    // owner-invite CLI executions too, and never fails the later upgrade from read to write.
    Ok(WriteTx(Some((
        pool.begin_with("BEGIN IMMEDIATE").await?,
        turn,
    ))))
}

pub async fn audit(
    db: &mut SqliteConnection,
    actor: &[u8],
    action: &str,
    person: Option<&[u8]>,
    id: Option<&str>,
    at: i64,
) -> ApiResult<i64> {
    Ok(query_scalar(
        "INSERT INTO audit_log (id, actor_person_id, action, target_person_id, target_id, at)
           SELECT COALESCE(MAX(id), 0) + 1, ?, ?, ?, ?, ? FROM audit_log RETURNING id",
    )
    .bind(actor)
    .bind(action)
    .bind(person)
    .bind(id)
    .bind(at)
    .fetch_one(db)
    .await?)
}

pub fn log_closed(sessions: Vec<i64>, at: i64, reason: &'static str) {
    for expires_at in sessions {
        let reason = if expires_at <= at {
            "token_expired"
        } else {
            reason
        };
        tracing::info!(reason, "session closed");
    }
}

pub async fn expire_records(app: crate::App) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        let result = async {
            let mut tx = write(&app.db).await?;
            let at = now()?;
            let sessions =
                query_scalar("DELETE FROM sessions WHERE expires_at <= ? RETURNING expires_at")
                    .bind(at)
                    .fetch_all(&mut *tx)
                    .await?;
            query("DELETE FROM open_joins WHERE expires_at <= ?")
                .bind(at)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            log_closed(sessions, at, "token_expired");
            Ok::<_, ApiError>(())
        }
        .await;
        if let Err(error) = result {
            tracing::error!(reason = error.code, "record expiry failed");
        }
        if let Err(error) = crate::calendar::expire(&app).await {
            tracing::error!(reason = error.code, "event expiry failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use axum::{
        Json,
        extract::{Path, State},
    };

    use super::*;
    use crate::{
        App, auth, channels, keys, members, metadata, presence,
        testing::{self, as_person},
        voice,
    };

    #[tokio::test]
    async fn fresh_main_and_preview_databases_reopen_with_both_features() -> Result<()> {
        for history in [
            "fresh",
            "main",
            "forum_preview",
            "forum",
            "calendar_preview",
            "calendar",
            "both_previews",
        ] {
            let dir = tempfile::tempdir()?;
            if history != "fresh" {
                let options = SqliteConnectOptions::new()
                    .filename(dir.path().join("rusttalk.sqlite3"))
                    .create_if_missing(true);
                let pool = SqlitePool::connect_with(options).await?;
                let mut old = sqlx::migrate!();
                let mut steps = Vec::new();
                for migration in old.iter() {
                    let include = migration.version <= 31
                        || match history {
                            "main" => migration.version == 32,
                            "forum_preview" | "both_previews" => migration.version == 36,
                            "forum" => matches!(migration.version, 32 | 36),
                            "calendar_preview" => matches!(migration.version, 32 | 34),
                            "calendar" => migration.version != 36,
                            _ => false,
                        }
                        || (history == "both_previews" && migration.version == 34);
                    if include {
                        let mut step = migration.clone();
                        if matches!(history, "forum_preview" | "both_previews")
                            && step.version == 36
                        {
                            step.version = 32;
                        }
                        steps.push(step);
                    }
                }
                if matches!(history, "calendar_preview" | "both_previews") {
                    steps.push(sqlx::migrate::Migration::new(
                        35,
                        "calendar delegation".into(),
                        sqlx::migrate::MigrationType::Simple,
                        include_str!("../preview/0035_calendar_delegation.sql").into(),
                        false,
                    ));
                }
                steps.sort_by_key(|m| m.version);
                old.migrations = steps.into();
                old.run(&pool).await?;
                if matches!(history, "forum_preview" | "forum" | "both_previews") {
                    query("INSERT INTO categories (id, position, name_ct) VALUES ('category', 0, X'03')").execute(&pool).await?;
                    query("INSERT INTO channels (id, kind, category_id, position, name_ct, forum) VALUES ('forum', 'text', 'category', 0, X'0102', 1)")
                        .execute(&pool).await?;
                }
                if history == "both_previews" {
                    query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 35")
                        .execute(&pool)
                        .await?;
                    assert!(open(dir.path()).await.is_err());
                    let moved: bool = query_scalar(
                        "SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version = 36)",
                    )
                    .fetch_one(&pool)
                    .await?;
                    assert!(
                        !moved,
                        "failed calendar upgrade must roll back forum renumbering"
                    );
                    let calendar = old.iter().find(|m| m.version == 35).unwrap();
                    query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 35")
                        .bind(calendar.checksum.as_ref())
                        .execute(&pool)
                        .await?;
                }
                if matches!(history, "forum_preview" | "both_previews") {
                    query("UPDATE _sqlx_migrations SET checksum = X'00' WHERE version = 32")
                        .execute(&pool)
                        .await?;
                    assert!(open(dir.path()).await.is_err());
                    let checksum: Vec<u8> =
                        query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version = 32")
                            .fetch_one(&pool)
                            .await?;
                    assert_eq!(checksum, [0]);
                    let forum = old.iter().find(|m| m.version == 32).unwrap();
                    query("UPDATE _sqlx_migrations SET checksum = ? WHERE version = 32")
                        .bind(forum.checksum.as_ref())
                        .execute(&pool)
                        .await?;
                }
                pool.close().await;
            }
            for _ in 0..2 {
                let pool = open(dir.path()).await?;
                query("SELECT forum FROM channels").execute(&pool).await?;
                if matches!(history, "forum_preview" | "forum" | "both_previews") {
                    let channel: (bool, Vec<u8>) =
                        sqlx::query_as("SELECT forum, name_ct FROM channels WHERE id = 'forum'")
                            .fetch_one(&pool)
                            .await?;
                    assert_eq!(channel, (true, vec![1, 2]));
                }
                query("SELECT reminded_at FROM calendar_events")
                    .execute(&pool)
                    .await?;
                query("SELECT envelope FROM calendar_answers")
                    .execute(&pool)
                    .await?;
                query("SELECT retained FROM calendar_role_grants")
                    .execute(&pool)
                    .await?;
                let count: i64 = query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
                    .fetch_one(&pool)
                    .await?;
                assert_eq!(count as usize, sqlx::migrate!().iter().count(), "{history}");
                query("SELECT system_messages_ct FROM server_settings")
                    .execute(&pool)
                    .await?;
                pool.close().await;
            }
        }
        Ok(())
    }

    const PEOPLE: u8 = 12;

    const VOICE: [u8; 32] = [9; 32];

    /// One request of the mix that people in a voice call with chat and presence produce.
    /// Returns whether it was a read.
    async fn request(app: &App, step: usize) -> ApiResult<bool> {
        let who = (step % PEOPLE as usize) as u8 + 1;
        let headers = as_person(who);
        let state = State(app.clone());
        let kind = step % 9;
        let result = match kind {
            0 => {
                let status = if step.is_multiple_of(2) {
                    "away"
                } else {
                    "online"
                };
                let choice = serde_json::from_value(serde_json::json!({ "status": status }))
                    .expect("valid status");
                presence::set(state, headers, Ok(Json(choice)))
                    .await
                    .map(drop)
            }
            1 => channels::catalog(state, headers).await.map(drop),
            2 => keys::state(state, headers).await.map(drop),
            3 => metadata::members(state, headers).await.map(drop),
            4 => members::me(state, headers).await.map(drop),
            5 => {
                // The owner reads every channel, so only the missing LiveKit refuses this.
                let channel = Path(rusttalk_core::auth::encode(&VOICE));
                voice::token(state, as_person(1), channel).await.map(drop)
            }
            6 => auth::challenge(state, headers).await.map(drop),
            _ => {
                // What an event socket reads when it opens and after a change.
                let (tx, actor) = auth::authenticate_read(app, &headers).await?;
                tx.commit().await?;
                let mut db = app.db.begin().await?;
                keys::pending(&mut db, &actor).await?;
                keys::scopes(&mut db, &actor).await?;
                presence::stored(&mut db, &actor.person).await?;
                db.commit().await?;
                Ok(())
            }
        };
        let expected = match kind {
            5 => Some("voice_unavailable"),
            6 => Some("challenge_limit"),
            _ => None,
        };
        match result {
            Err(error) if Some(error.code) == expected => {}
            other => other?,
        }
        Ok(!matches!(kind, 0 | 6))
    }

    /// A write that outlasts SQLite's busy timeout, as a commit on a stalled disk does, must
    /// only delay other requests: readers never wait for it and writers queue behind it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn a_slow_write_fails_no_concurrent_request() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = testing::app(dir.path(), PEOPLE).await?;
        query("INSERT INTO categories (id, position, name_ct) VALUES ('c', 0, x'00')")
            .execute(&app.db)
            .await?;
        query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, 'voice', 'c', 0, x'00')")
            .bind(rusttalk_core::auth::encode(&VOICE))
            .execute(&app.db)
            .await?;
        let stalled = Arc::new(AtomicBool::new(true));
        let reads_during_stall = Arc::new(AtomicUsize::new(0));
        let mut tx = write(&app.db).await?;
        query("UPDATE members SET presence_status = 'away' WHERE person_id = ?")
            .bind(testing::person(1))
            .execute(&mut *tx)
            .await?;
        let stall = tokio::spawn({
            let stalled = stalled.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(6)).await;
                let committed = tx.commit().await;
                stalled.store(false, Ordering::SeqCst);
                committed
            }
        });
        let tasks: Vec<_> = (0..48)
            .map(|task| {
                let (app, stalled) = (app.clone(), stalled.clone());
                let reads = reads_during_stall.clone();
                tokio::spawn(async move {
                    let (mut failures, mut ok, mut step) = (Vec::new(), 0, task);
                    // Keep the mix going through the stall and for a while after it.
                    while stalled.load(Ordering::SeqCst) || ok < 20 {
                        match request(&app, step).await {
                            Ok(read) => {
                                if read && stalled.load(Ordering::SeqCst) {
                                    reads.fetch_add(1, Ordering::SeqCst);
                                }
                                ok += 1;
                            }
                            Err(error) => failures.push(error.code),
                        }
                        step += 48;
                    }
                    failures
                })
            })
            .collect();
        let mut failures = BTreeMap::new();
        for task in tasks {
            for code in task.await? {
                *failures.entry(code).or_insert(0) += 1;
            }
        }
        stall.await??;
        assert!(failures.is_empty(), "failed requests: {failures:?}");
        assert!(
            reads_during_stall.load(Ordering::SeqCst) > 0,
            "reads waited for the write"
        );
        Ok(())
    }

    /// The same database with no busy timeout, so a writer that reaches SQLite while another
    /// connection still holds the write lock fails at once instead of waiting.
    async fn impatient(dir: &std::path::Path) -> anyhow::Result<SqlitePool> {
        let options = SqliteConnectOptions::new()
            .filename(dir.join("rusttalk.sqlite3"))
            .busy_timeout(Duration::ZERO);
        Ok(SqlitePoolOptions::new().connect_with(options).await?)
    }

    async fn next_write_succeeds(pool: &SqlitePool) -> ApiResult<()> {
        let mut tx = write(pool).await?;
        query("UPDATE server_settings SET open_joining = open_joining")
            .execute(&mut *tx)
            .await?;
        Ok(tx.commit().await?)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_statement_keeps_the_turn_until_it_rolls_back() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Kept open: closing the last connection checkpoints and locks the database.
        let _schema = open(dir.path()).await?;
        let pool = impatient(dir.path()).await?;
        // The next writer comes from elsewhere, so it cannot queue behind on the same connection.
        let next = impatient(dir.path()).await?;
        query("SELECT 1").execute(&next).await?;
        let mut tx = write(&pool).await?;
        let (started, running) = tokio::sync::oneshot::channel();
        let mut started = Some(started);
        tx.lock_handle().await?.set_progress_handler(1000, move || {
            if let Some(started) = started.take() {
                let _ = started.send(());
            }
            true
        });
        let slow = async move {
            query(
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 5000000)
                 SELECT COUNT(*) FROM n",
            )
            .execute(&mut *tx)
            .await
        };
        // The request goes away once SQLite is running its statement.
        tokio::select! {
            _ = slow => panic!("the statement finished before the request went away"),
            _ = running => {}
        }
        next_write_succeeds(&next).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_commit_keeps_the_turn_until_it_finishes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Kept open: closing the last connection checkpoints and locks the database.
        let _schema = open(dir.path()).await?;
        let pool = impatient(dir.path()).await?;
        // The next writer comes from elsewhere, so it cannot queue behind on the same connection.
        let next = impatient(dir.path()).await?;
        query("SELECT 1").execute(&next).await?;
        query("CREATE TABLE bulk (data BLOB)")
            .execute(&pool)
            .await?;
        let mut tx = write(&pool).await?;
        query("INSERT INTO bulk VALUES (x'00')")
            .execute(&mut *tx)
            .await?;
        // A commit that stalls while holding the write lock, as a slow disk sync does.
        tx.lock_handle().await?.set_commit_hook(|| {
            std::thread::sleep(Duration::from_millis(500));
            true
        });
        // The request goes away while SQLite still commits.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), tx.commit())
                .await
                .is_err()
        );
        next_write_succeeds(&next).await?;
        let rows: i64 = query_scalar("SELECT COUNT(*) FROM bulk")
            .fetch_one(&pool)
            .await?;
        assert_eq!(rows, 1, "the cancelled commit still completes");
        Ok(())
    }
}
