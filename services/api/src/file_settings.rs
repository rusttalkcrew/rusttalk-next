use anyhow::Result;
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::HeaderMap,
};
use rusttalk_core::auth::{FileLimits, FileSettings};
use sqlx::{SqliteConnection, query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    files::setting,
    payload,
    roles::{self, Permission},
};

pub fn ceilings() -> Result<FileLimits> {
    Ok(FileLimits {
        max_file_bytes: setting("RUSTTALK_FILE_MAX_BYTES", 32 * 1024 * 1024)?,
        member_storage_bytes: setting("RUSTTALK_FILE_MEMBER_BYTES", 500 * 1024 * 1024)?,
        server_storage_bytes: setting("RUSTTALK_FILE_SERVER_BYTES", 5 * 1024 * 1024 * 1024)?,
        retention_secs: setting("RUSTTALK_FILE_TTL_SECS", 30 * 24 * 60 * 60)?,
        max_stickers: setting("RUSTTALK_STICKER_MAX", 50)?,
    })
}

pub async fn effective(db: &mut SqliteConnection, ceilings: FileLimits) -> ApiResult<FileLimits> {
    let (max_file_bytes, member_storage_bytes, server_storage_bytes, retention_secs, max_stickers) =
        query_as::<
            _,
            (
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
                Option<i64>,
            ),
        >(
            "SELECT max_file_bytes, member_storage_bytes, server_storage_bytes, retention_secs,
             max_stickers FROM server_settings WHERE id = 1",
        )
        .fetch_one(db)
        .await?;
    let limit = |saved: Option<i64>, ceiling| saved.unwrap_or(ceiling).min(ceiling);
    Ok(FileLimits {
        max_file_bytes: limit(max_file_bytes, ceilings.max_file_bytes),
        member_storage_bytes: limit(member_storage_bytes, ceilings.member_storage_bytes),
        server_storage_bytes: limit(server_storage_bytes, ceilings.server_storage_bytes),
        retention_secs: limit(retention_secs, ceilings.retention_secs),
        max_stickers: limit(max_stickers, ceilings.max_stickers),
    })
}

pub async fn used_bytes(db: &mut SqliteConnection) -> ApiResult<i64> {
    Ok(query_scalar("SELECT COALESCE(SUM(size), 0) FROM files")
        .fetch_one(db)
        .await?)
}

pub async fn get(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<FileSettings>> {
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let ceilings = app.files.ceilings;
    let limits = effective(&mut tx, ceilings).await?;
    let used_bytes = used_bytes(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(FileSettings {
        limits,
        ceilings,
        used_bytes,
    }))
}

pub async fn set(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<FileLimits>, JsonRejection>,
) -> ApiResult<Json<FileSettings>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageServer)?;
    let requested = payload(body)?;
    if [
        requested.max_file_bytes,
        requested.member_storage_bytes,
        requested.server_storage_bytes,
        requested.retention_secs,
        requested.max_stickers,
    ]
    .iter()
    .any(|&value| value <= 0)
    {
        return Err(ApiError::bad_request("invalid_file_limits"));
    }
    let ceilings = app.files.ceilings;
    let limits = FileLimits {
        max_file_bytes: requested.max_file_bytes.min(ceilings.max_file_bytes),
        member_storage_bytes: requested
            .member_storage_bytes
            .min(ceilings.member_storage_bytes),
        server_storage_bytes: requested
            .server_storage_bytes
            .min(ceilings.server_storage_bytes),
        retention_secs: requested.retention_secs.min(ceilings.retention_secs),
        max_stickers: requested.max_stickers.min(ceilings.max_stickers),
    };
    query(
        "UPDATE server_settings SET max_file_bytes = ?, member_storage_bytes = ?,
         server_storage_bytes = ?, retention_secs = ?, max_stickers = ? WHERE id = 1",
    )
    .bind(limits.max_file_bytes)
    .bind(limits.member_storage_bytes)
    .bind(limits.server_storage_bytes)
    .bind(limits.retention_secs)
    .bind(limits.max_stickers)
    .execute(&mut *tx)
    .await?;
    let used_bytes = used_bytes(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(FileSettings {
        limits,
        ceilings,
        used_bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person, person};

    #[tokio::test]
    async fn members_can_read_but_only_managers_can_save_and_ceilings_apply_twice()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut app = app(dir.path(), 2).await?;
        let host = FileLimits {
            max_file_bytes: 32,
            member_storage_bytes: 64,
            server_storage_bytes: 128,
            retention_secs: 60,
            max_stickers: 8,
        };
        std::sync::Arc::get_mut(&mut app.files).unwrap().ceilings = host;
        assert_eq!(
            get(State(app.clone()), HeaderMap::new())
                .await
                .unwrap_err()
                .code,
            "invalid_session"
        );
        let Json(initial) = get(State(app.clone()), as_person(2)).await?;
        assert_eq!(initial.limits, host);
        assert_eq!(initial.ceilings, host);
        assert_eq!(initial.used_bytes, 0);
        let requested = FileLimits {
            max_file_bytes: 99,
            member_storage_bytes: 48,
            server_storage_bytes: 96,
            retention_secs: 55,
            max_stickers: 6,
        };
        let error = set(State(app.clone()), as_person(2), Ok(Json(requested)))
            .await
            .unwrap_err();
        assert_eq!(error.code, "permission_denied");
        query("INSERT INTO roles (id, kind, rank, permissions) VALUES ('storage', 'custom', 1, ?)")
            .bind(Permission::ManageServer as i64)
            .execute(&app.db)
            .await?;
        query("INSERT INTO member_roles (person_id, role_id) VALUES (?, 'storage')")
            .bind(person(2))
            .execute(&app.db)
            .await?;

        let invalid = FileLimits {
            retention_secs: 0,
            ..requested
        };
        let error = set(State(app.clone()), as_person(2), Ok(Json(invalid)))
            .await
            .unwrap_err();
        assert_eq!(error.code, "invalid_file_limits");
        let Json(saved) = set(State(app.clone()), as_person(2), Ok(Json(requested))).await?;
        assert_eq!(
            saved.limits,
            FileLimits {
                max_file_bytes: 32,
                ..requested
            }
        );
        let stored: i64 = query_scalar("SELECT max_file_bytes FROM server_settings WHERE id = 1")
            .fetch_one(&app.db)
            .await?;
        assert_eq!(stored, 32);

        let lowered_host = FileLimits {
            max_file_bytes: 24,
            member_storage_bytes: 42,
            server_storage_bytes: 84,
            retention_secs: 30,
            max_stickers: 3,
        };
        std::sync::Arc::get_mut(&mut app.files).unwrap().ceilings = lowered_host;
        let Json(after) = get(State(app), as_person(2)).await?;
        assert_eq!(after.limits, lowered_host);
        assert_eq!(after.ceilings, lowered_host);
        Ok(())
    }
}
