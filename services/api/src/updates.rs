use std::{io::ErrorKind, path::Path};

use axum::{Json, extract::State, http::HeaderMap};
use rusttalk_core::auth::ServerUpdates;

use crate::{ApiError, ApiResult, App, auth};

pub async fn status(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<ServerUpdates>> {
    read_status(&app, &headers, Path::new("/update-status/status.json")).await
}

async fn read_status(
    app: &App,
    headers: &HeaderMap,
    path: &Path,
) -> ApiResult<Json<ServerUpdates>> {
    let (mut tx, actor) = auth::authenticate(app, headers).await?;
    auth::require_owner(&mut tx, &actor.person).await?;
    tx.commit().await?;
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Json)
            .map_err(|_| ApiError::internal("invalid_update_status")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Json(ServerUpdates::default())),
        Err(_) => Err(ApiError::internal("update_status_unavailable")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person};

    #[tokio::test]
    async fn only_the_owner_can_read_update_status() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let path = dir.path().join("updates.json");
        assert!(
            read_status(&app, &as_person(1), &path)
                .await?
                .pending
                .is_none()
        );
        tokio::fs::write(&path, br#"{"pending":{"version":"0.1.4","since":100}}"#).await?;
        let Json(status) = read_status(&app, &as_person(1), &path).await?;
        assert_eq!(status.pending.unwrap().version, "0.1.4");
        assert!(matches!(
            read_status(&app, &as_person(2), &path).await,
            Err(error) if error.status == axum::http::StatusCode::FORBIDDEN
        ));
        Ok(())
    }
}
