use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
};
use futures_util::TryStreamExt;
use rusttalk_core::sync::{MAX_BODY, split_blob};
use sqlx::{query, query_as, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    files,
};

pub async fn get(State(app): State<App>, headers: HeaderMap) -> ApiResult<Response> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
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

/// Reads at most `MAX_BODY` bytes. A larger body is read to its end before the 413, so the
/// proxy does not report a reset stream as 502.
async fn read_body(body: Body) -> ApiResult<Vec<u8>> {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|_| ApiError::bad_request("invalid_sync_blob"))?
    {
        if chunk.len() > MAX_BODY - bytes.len() {
            files::drain(&mut stream).await;
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub async fn put(State(app): State<App>, headers: HeaderMap, body: Body) -> ApiResult<Response> {
    let body = read_body(body).await?;
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
