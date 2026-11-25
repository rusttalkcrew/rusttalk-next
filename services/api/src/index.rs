use std::time::Duration;

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::{Fingerprint, decode, encode},
    index::{
        Listing, Registration, https_url, now_ms, open_settings, seal_settings, validate_details,
    },
    pinned_tls::https_client,
};
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, query, query_scalar};
use tokio::sync::watch;

use crate::{ApiError, ApiResult, App, auth, payload, store::audit_details};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    enabled: bool,
    listing: Option<Settings>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    name: String,
    description: String,
    address: String,
    index_url: String,
    index_tls_fingerprint: Option<String>,
}

pub async fn get(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<Publication>> {
    let (mut tx, actor) = auth::authenticate_read(&app, &headers).await?;
    auth::require_owner(&mut tx, &actor.person).await?;
    let listing = settings(&app, &mut tx).await?;
    tx.commit().await?;
    Ok(Json(Publication {
        enabled: listing.is_some(),
        listing,
    }))
}

pub async fn update(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Publication>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = auth::authenticate(&app, &headers).await?;
    auth::require_owner(&mut tx, &actor.person).await?;
    let Publication { enabled, listing } = payload(body)?;
    let ciphertext = match (enabled, listing) {
        (false, None) => None,
        (true, Some(settings)) => {
            validate_details(&settings.name, &settings.description, &settings.address)
                .map_err(|_| ApiError::bad_request("invalid_listing"))?;
            https_url(&settings.index_url).ok_or(ApiError::bad_request("invalid_index_url"))?;
            pin(&settings)?;
            let clear = serde_json::to_vec(&settings)
                .map_err(|_| ApiError::internal("listing_settings_encode"))?;
            Some(
                seal_settings(&app.key, &clear)
                    .map_err(|_| ApiError::internal("listing_settings_encrypt"))?,
            )
        }
        _ => return Err(ApiError::bad_request("invalid_listing_settings")),
    };
    query("UPDATE server_settings SET index_listing_ct = ? WHERE id = 1")
        .bind(ciphertext)
        .execute(&mut *tx)
        .await?;
    // The public name and address are left out: only whether it is listed.
    audit_details(
        &mut tx,
        &actor.person,
        "listing_changed",
        None,
        None,
        Some(serde_json::json!({ "enabled": enabled })),
        actor.at,
    )
    .await?;
    tx.commit().await?;
    app.listing_changed.send_replace(());
    Ok(StatusCode::NO_CONTENT)
}

fn pin(settings: &Settings) -> ApiResult<Option<Fingerprint>> {
    settings
        .index_tls_fingerprint
        .as_deref()
        .map(decode)
        .transpose()
        .map_err(|_| ApiError::bad_request("invalid_index_tls_fingerprint"))
}

async fn settings(app: &App, db: &mut SqliteConnection) -> ApiResult<Option<Settings>> {
    let ciphertext: Option<Vec<u8>> =
        query_scalar("SELECT index_listing_ct FROM server_settings WHERE id = 1")
            .fetch_one(db)
            .await?;
    ciphertext
        .map(|ciphertext| {
            let clear = open_settings(&app.key, &ciphertext)
                .map_err(|_| ApiError::internal("listing_settings_decrypt"))?;
            serde_json::from_slice(&clear)
                .map_err(|_| ApiError::internal("listing_settings_decode"))
        })
        .transpose()
}

const REFRESH: Duration = Duration::from_secs(3_600);
const RETRY: Duration = Duration::from_secs(300);

pub async fn refresh(app: App, mut changed: watch::Receiver<()>) {
    let mut last_timestamp = 0;
    loop {
        tokio::select! {
            biased;
            update = changed.changed() => {
                if update.is_err() {
                    return;
                }
            }
            () = async {
                // A failed attempt retries sooner so a short outage does not use up the lease.
                let delay = match publish(&app, &mut last_timestamp).await {
                    Ok(()) => REFRESH,
                    Err(error) => {
                        tracing::warn!(reason = error.code, "index registration failed");
                        RETRY
                    }
                };
                tokio::time::sleep(delay).await;
            } => {}
        }
    }
}

async fn publish(app: &App, last_timestamp: &mut i64) -> ApiResult<()> {
    let Some(settings) = settings(app, &mut *app.db.acquire().await?).await? else {
        return Ok(());
    };
    let client =
        https_client(pin(&settings)?).map_err(|_| ApiError::internal("index_http_client"))?;
    let base = https_url(&settings.index_url).ok_or(ApiError::internal("invalid_index_url"))?;
    let url = format!("{}/v1/servers", base.as_str().trim_end_matches('/'));
    let member_count = query_scalar("SELECT COUNT(*) FROM members")
        .fetch_one(&app.db)
        .await?;
    let open_joining = query_scalar("SELECT open_joining FROM server_settings WHERE id = 1")
        .fetch_one(&app.db)
        .await?;
    let listing = Listing {
        server_id: encode(app.key.verifying_key().as_bytes()),
        name: settings.name,
        description: settings.description,
        address: settings.address,
        member_count,
        open_joining,
    };
    let now = now_ms().ok_or(ApiError::internal("clock_error"))?;
    let timestamp = now.max(*last_timestamp + 1);
    let record = Registration::signed(listing, timestamp, &app.key)
        .map_err(|_| ApiError::internal("index_signing_failed"))?;
    *last_timestamp = timestamp;
    let response = client
        .post(url)
        .json(&record)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|error| {
            ApiError::internal(if error.is_timeout() {
                "index_timeout"
            } else if error.is_connect() {
                "index_unreachable"
            } else {
                "index_request_failed"
            })
        })?;
    let status = response.status();
    if status == StatusCode::NO_CONTENT {
        return Ok(());
    }
    let mut response = response;
    let mut body = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while let Ok(Some(chunk)) = response.chunk().await {
            body.extend_from_slice(&chunk);
            if body.len() > 1_024 {
                body.clear();
                break;
            }
        }
    })
    .await;
    tracing::warn!(
        status = status.as_u16(),
        index_error = index_error(&body),
        "index rejected the registration"
    );
    Err(ApiError::internal("index_rejected_registration"))
}

/// Extract the index's short error code, ignoring anything that is not a plain code.
fn index_error(body: &[u8]) -> &str {
    #[derive(Deserialize)]
    struct Body<'a> {
        error: &'a str,
    }
    serde_json::from_slice::<Body>(body)
        .ok()
        .map(|body| body.error)
        .filter(|code| {
            (1..=64).contains(&code.len())
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        })
        .unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person};

    #[tokio::test]
    async fn only_the_owner_changes_the_listing_and_it_is_audited() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let off = || {
            Ok(Json(Publication {
                enabled: false,
                listing: None,
            }))
        };
        let refused = update(State(app.clone()), as_person(2), off()).await;
        assert_eq!(refused.unwrap_err().status, StatusCode::FORBIDDEN);
        update(State(app.clone()), as_person(1), off()).await?;
        let details: Vec<String> =
            query_scalar("SELECT details FROM audit_log WHERE action = 'listing_changed'")
                .fetch_all(&app.db)
                .await?;
        assert_eq!(details, [r#"{"enabled":false}"#]);
        Ok(())
    }

    #[test]
    fn index_error_codes_are_short_plain_codes() {
        assert_eq!(
            index_error(br#"{"error":"stale_registration"}"#),
            "stale_registration"
        );
        for body in [
            &br#"{"error":"x\ny"}"#[..],
            br#"{"error":"Bearer abc"}"#,
            br#"{"error":""}"#,
            b"<html>",
            b"",
        ] {
            assert_eq!(index_error(body), "unknown");
        }
        let long = format!(r#"{{"error":"{}"}}"#, "a".repeat(65));
        assert_eq!(index_error(long.as_bytes()), "unknown");
    }
}
