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
    Sqlite, SqliteConnection, SqlitePool, Transaction, query, query_scalar,
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
    sqlx::migrate!().run(&pool).await?;
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

pub async fn write(pool: &SqlitePool) -> ApiResult<Transaction<'static, Sqlite>> {
    // shortcut: every authenticated request takes SQLite's single write lock, fine for small
    // communities; move read-only handlers to deferred transactions if busy errors show up.
    // Serialize authorization and mutation with other requests and owner-invite CLI executions.
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
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

pub async fn expire_records(pool: SqlitePool) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    loop {
        interval.tick().await;
        let result = async {
            let at = now()?;
            let sessions =
                query_scalar("DELETE FROM sessions WHERE expires_at <= ? RETURNING expires_at")
                    .bind(at)
                    .fetch_all(&pool)
                    .await?;
            log_closed(sessions, at, "token_expired");
            query("DELETE FROM open_joins WHERE expires_at <= ?")
                .bind(at)
                .execute(&pool)
                .await?;
            Ok::<_, ApiError>(())
        }
        .await;
        if let Err(error) = result {
            tracing::error!(reason = error.code, "record expiry failed");
        }
    }
}
