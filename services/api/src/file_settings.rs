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
    files::{Files, optional_setting},
    payload,
    roles::{self, Permission},
    store::audit_details,
};

/// Limits that apply until someone with Manage Server saves their own.
const DEFAULTS: FileLimits = FileLimits {
    max_file_bytes: 32 * 1024 * 1024,
    member_storage_bytes: 500 * 1024 * 1024,
    server_storage_bytes: 5 * 1024 * 1024 * 1024,
    retention_secs: 30 * 24 * 60 * 60,
    max_stickers: 50,
};

/// The host's ceilings and the defaults. A ceiling exists only when the operator set
/// its variable, which is then also the default (as it always was). Without one the
/// owner may choose anything up to `FileLimits::MAX`.
pub fn host() -> Result<(FileLimits, FileLimits)> {
    host_from(optional_setting)
}

fn host_from(setting: impl Fn(&str) -> Result<Option<i64>>) -> Result<(FileLimits, FileLimits)> {
    let pick = |name: &str, default: i64, max: i64| -> Result<(i64, i64)> {
        Ok(match setting(name)? {
            Some(value) => {
                if value > max {
                    tracing::warn!(
                        name,
                        max,
                        "file limit above what RustTalk supports; using the maximum"
                    );
                }
                let value = value.min(max);
                (value, value)
            }
            None => (max, default),
        })
    };
    let (max, defaults) = (FileLimits::MAX, DEFAULTS);
    let file = pick(
        "RUSTTALK_FILE_MAX_BYTES",
        defaults.max_file_bytes,
        max.max_file_bytes,
    )?;
    let member = pick(
        "RUSTTALK_FILE_MEMBER_BYTES",
        defaults.member_storage_bytes,
        max.member_storage_bytes,
    )?;
    let server = pick(
        "RUSTTALK_FILE_SERVER_BYTES",
        defaults.server_storage_bytes,
        max.server_storage_bytes,
    )?;
    let retention = pick(
        "RUSTTALK_FILE_TTL_SECS",
        defaults.retention_secs,
        max.retention_secs,
    )?;
    let stickers = pick(
        "RUSTTALK_STICKER_MAX",
        defaults.max_stickers,
        max.max_stickers,
    )?;
    let limits = |pick: fn((i64, i64)) -> i64| FileLimits {
        max_file_bytes: pick(file),
        member_storage_bytes: pick(member),
        server_storage_bytes: pick(server),
        retention_secs: pick(retention),
        max_stickers: pick(stickers),
    };
    Ok((
        limits(|(ceiling, _)| ceiling),
        limits(|(_, default)| default),
    ))
}

pub async fn effective(db: &mut SqliteConnection, files: &Files) -> ApiResult<FileLimits> {
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
    let (ceilings, defaults) = (files.ceilings, files.defaults);
    let limit = |saved: Option<i64>, default: i64, ceiling| saved.unwrap_or(default).min(ceiling);
    Ok(FileLimits {
        max_file_bytes: limit(
            max_file_bytes,
            defaults.max_file_bytes,
            ceilings.max_file_bytes,
        ),
        member_storage_bytes: limit(
            member_storage_bytes,
            defaults.member_storage_bytes,
            ceilings.member_storage_bytes,
        ),
        server_storage_bytes: limit(
            server_storage_bytes,
            defaults.server_storage_bytes,
            ceilings.server_storage_bytes,
        ),
        retention_secs: limit(
            retention_secs,
            defaults.retention_secs,
            ceilings.retention_secs,
        ),
        max_stickers: limit(max_stickers, defaults.max_stickers, ceilings.max_stickers),
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
    let limits = effective(&mut tx, &app.files).await?;
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
    let before = effective(&mut tx, &app.files).await?;
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
    if limits != before {
        audit_details(
            &mut tx,
            &actor.person,
            "file_limits_changed",
            None,
            None,
            Some(serde_json::json!({ "before": before, "after": limits })),
            actor.at,
        )
        .await?;
    }
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

    /// What the host sets through the environment: ceilings that are also the defaults.
    fn set_host(app: &mut App, limits: FileLimits) {
        let files = std::sync::Arc::get_mut(&mut app.files).unwrap();
        files.ceilings = limits;
        files.defaults = limits;
    }

    #[test]
    fn host_ceilings_exist_only_when_the_operator_sets_them() -> anyhow::Result<()> {
        assert_eq!(host_from(|_| Ok(None))?, (FileLimits::MAX, DEFAULTS));
        let (ceilings, defaults) = host_from(|name| {
            Ok(match name {
                "RUSTTALK_FILE_SERVER_BYTES" => Some(10 << 30),
                // More than one attachment can hold: the technical maximum applies.
                "RUSTTALK_FILE_MAX_BYTES" => Some(64 << 20),
                _ => None,
            })
        })?;
        let expected = FileLimits {
            server_storage_bytes: 10 << 30,
            ..FileLimits::MAX
        };
        assert_eq!(ceilings, expected);
        assert_eq!(
            defaults,
            FileLimits {
                server_storage_bytes: 10 << 30,
                max_file_bytes: FileLimits::MAX.max_file_bytes,
                ..DEFAULTS
            }
        );
        assert!(host_from(|_| Err(anyhow::anyhow!("invalid"))).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn without_host_ceilings_owners_choose_any_limit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 1).await?;
        let Json(initial) = get(State(app.clone()), as_person(1)).await?;
        assert_eq!(initial.limits, DEFAULTS);
        assert_eq!(initial.ceilings, FileLimits::MAX);

        let generous = FileLimits {
            max_file_bytes: 64 << 20,
            member_storage_bytes: 50 << 30,
            server_storage_bytes: 2 << 40,
            retention_secs: 3650 * 86_400,
            max_stickers: 5000,
        };
        let Json(saved) = set(State(app.clone()), as_person(1), Ok(Json(generous))).await?;
        assert_eq!(
            saved.limits,
            FileLimits {
                max_file_bytes: FileLimits::MAX.max_file_bytes,
                ..generous
            }
        );
        let Json(saved) = set(State(app.clone()), as_person(1), Ok(Json(FileLimits::MAX))).await?;
        assert_eq!(saved.limits, FileLimits::MAX);
        // The largest values still work in the expiry sweep without overflowing.
        crate::files::expire_batch(&app).await?;
        Ok(())
    }

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
        set_host(&mut app, host);
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
        // The audit log keeps the limits before and after, and nothing for an unchanged save.
        let _ = set(State(app.clone()), as_person(2), Ok(Json(requested))).await?;
        let details: Vec<String> = query_scalar(
            "SELECT details FROM audit_log WHERE action = 'file_limits_changed' AND actor_person_id = ?",
        )
        .bind(person(2))
        .fetch_all(&app.db)
        .await?;
        assert_eq!(details.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&details[0])?,
            serde_json::json!({ "before": host, "after": saved.limits })
        );

        let lowered_host = FileLimits {
            max_file_bytes: 24,
            member_storage_bytes: 42,
            server_storage_bytes: 84,
            retention_secs: 30,
            max_stickers: 3,
        };
        set_host(&mut app, lowered_host);
        let Json(after) = get(State(app), as_person(2)).await?;
        assert_eq!(after.limits, lowered_host);
        assert_eq!(after.ceilings, lowered_host);
        Ok(())
    }
}
