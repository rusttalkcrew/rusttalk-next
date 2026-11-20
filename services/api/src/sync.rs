use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use rusttalk_core::sync::split_blob;
use sqlx::{query, query_as, query_scalar};

use crate::{ApiError, ApiResult, App, auth::authenticate};

pub async fn get(State(app): State<App>, headers: HeaderMap) -> ApiResult<Response> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let row: Option<(i64, Vec<u8>)> =
        query_as("SELECT version, ciphertext FROM device_sync WHERE person_id = ?")
            .bind(actor.person.as_slice())
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(match row {
        Some((version, ciphertext)) => binary(
            StatusCode::OK,
            [version.to_be_bytes().as_slice(), &ciphertext].concat(),
        ),
        None => StatusCode::NO_CONTENT.into_response(),
    })
}

pub async fn put(State(app): State<App>, headers: HeaderMap, body: Bytes) -> ApiResult<Response> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let (version, ciphertext) =
        split_blob(&body).ok_or(ApiError::bad_request("invalid_sync_blob"))?;
    let written = query(
        "INSERT INTO device_sync (person_id, version, ciphertext, size, at) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (person_id) DO UPDATE SET version = excluded.version,
           ciphertext = excluded.ciphertext, size = excluded.size, at = excluded.at
         WHERE excluded.version > device_sync.version",
    )
    .bind(actor.person.as_slice())
    .bind(version)
    .bind(ciphertext)
    .bind(ciphertext.len() as i64)
    .bind(actor.at)
    .execute(&mut *tx)
    .await?;
    if written.rows_affected() == 0 {
        let stored: i64 = query_scalar("SELECT version FROM device_sync WHERE person_id = ?")
            .bind(actor.person.as_slice())
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(binary(StatusCode::CONFLICT, stored.to_be_bytes().to_vec()));
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn binary(status: StatusCode, body: Vec<u8>) -> Response {
    (status, [(CONTENT_TYPE, "application/octet-stream")], body).into_response()
}
