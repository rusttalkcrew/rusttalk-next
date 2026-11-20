mod auth;
mod channels;
mod dms;
mod events;
mod files;
mod index;
mod keys;
mod members;
mod messages;
mod metadata;
mod presence;
mod roles;
mod store;
mod sync;
mod voice;

use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
};

use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{Request, rejection::JsonRejection},
    http::{StatusCode, header::CACHE_CONTROL},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use ed25519_dalek::SigningKey;
use rusttalk_core::keys::Event;
use rusttalk_core::{
    auth::{ProtocolError, encode},
    identity::fingerprint,
};
use serde_json::json;
use sqlx::SqlitePool;
use tokio::{
    net::TcpListener,
    sync::{broadcast, watch},
};
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
};
use tracing::{Instrument, Level};
use tracing_subscriber::{filter::Targets, prelude::*};

#[derive(Clone)]
struct App {
    db: SqlitePool,
    key: Arc<SigningKey>,
    changes: broadcast::Sender<Option<rusttalk_core::presence::Update>>,
    presence: Arc<presence::Sessions>,
    voice: Option<Arc<voice::LiveKit>>,
    listing_changed: watch::Sender<()>,
    files: Arc<files::Files>,
}

impl App {
    fn notify(&self, event: Option<Event>) {
        let _ = self
            .changes
            .send(event.map(rusttalk_core::presence::Update::Event));
    }
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }
    fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }
    fn unauthorized(code: &'static str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code)
    }
    fn forbidden(code: &'static str) -> Self {
        Self::new(StatusCode::FORBIDDEN, code)
    }
    fn not_found(code: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, code)
    }
    fn internal(code: &'static str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}

impl std::error::Error for ApiError {}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        let code = match error {
            sqlx::Error::PoolTimedOut => "database_pool_timeout",
            sqlx::Error::Database(ref error) if error.code().as_deref() == Some("5") => {
                "database_busy"
            }
            _ => "database_error",
        };
        Self::internal(code)
    }
}

impl From<ProtocolError> for ApiError {
    fn from(_: ProtocolError) -> Self {
        Self::bad_request("invalid_encoding")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::error!(reason = self.code, "request failed");
        } else {
            tracing::warn!(reason = self.code, "request rejected");
        }
        (self.status, Json(json!({ "error": self.code }))).into_response()
    }
}

fn payload<T>(body: Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    body.map(|Json(value)| value)
        .map_err(|error| match error.status() {
            StatusCode::PAYLOAD_TOO_LARGE => ApiError::new(error.status(), "body_too_large"),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => ApiError::new(error.status(), "expected_json"),
            _ => ApiError::bad_request("invalid_json"),
        })
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::registry()
        .with(
            Targets::new()
                .with_default(Level::WARN)
                .with_target("rusttalk_", Level::INFO),
        )
        .with(tracing_subscriber::fmt::layer().json())
        .init();
    if let Err(error) = run().await {
        tracing::error!(error = %format_args!("{error:#}"), "API command failed");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run() -> Result<()> {
    let dir =
        env::var_os("RUSTTALK_DATA_DIR").map_or_else(|| PathBuf::from("/data"), PathBuf::from);
    let args: Vec<_> = env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => {
            let db = store::open(&dir).await?;
            let (key, created) = store::server_key(&dir)?;
            if created && auth::owner_pending(&db).await? {
                tracing::info!(
                    "Claim ownership with docker compose exec api rusttalk-api owner-invite"
                );
            }
            let files = Arc::new(files::Files::open(&dir, &db).await?);
            serve(App {
                files,
                db,
                key: Arc::new(key),
                changes: broadcast::channel(128).0,
                presence: Arc::default(),
                listing_changed: watch::channel(()).0,
                voice: voice::LiveKit::from_env()?.map(Arc::new),
            })
            .await?;
        }
        [cmd] if cmd == "owner-invite" => {
            if let Some(code) = auth::owner_invite(&store::open(&dir).await?).await? {
                writeln!(io::stdout().lock(), "{code}")?;
            }
        }
        [cmd] if cmd == "server-id" || cmd == "server-fingerprint" => {
            let (key, _) = store::server_key(&dir)?;
            let id = key.verifying_key().to_bytes();
            let value = if cmd == "server-id" {
                encode(&id)
            } else {
                fingerprint(&id)
            };
            writeln!(io::stdout().lock(), "{value}")?;
        }
        _ => bail!("usage: rusttalk-api [owner-invite|server-id|server-fingerprint]"),
    }
    Ok(())
}

async fn serve(app: App) -> io::Result<()> {
    let expiry = tokio::spawn(store::expire_records(app.db.clone()));
    let publisher = tokio::spawn(index::refresh(app.clone(), app.listing_changed.subscribe()));
    let file_expiry = tokio::spawn(files::expire(app.clone()));
    let app = Router::new()
        .route("/v1/health", get(|| async { "ok\n" }))
        .route("/v1/server-info", get(members::server_info))
        .route(
            "/v1/settings/open-joining",
            get(members::open_joining).put(members::set_open_joining),
        )
        .route("/v1/auth/join", post(auth::open_join))
        .route("/v1/auth/challenge", get(auth::challenge))
        .route("/v1/auth/sign-in", post(auth::sign_in))
        .route("/v1/auth/refresh", post(auth::refresh))
        .route("/v1/me", get(members::me))
        .route("/v1/me/status", put(presence::set))
        .route("/v1/me/membership", delete(members::leave))
        .route("/v1/space", get(metadata::space))
        .route("/v1/members", get(metadata::members))
        .route("/v1/events", get(events::upgrade))
        .route("/v1/keys", get(keys::state).post(keys::upload))
        .route("/v1/keys/epochs", post(keys::new_epoch))
        .route("/v1/keys/reject", post(keys::reject))
        .route("/v1/member-devices", get(keys::devices))
        .route("/v1/invite-preview", post(members::preview))
        .route("/v1/catalog", get(channels::catalog))
        .route("/v1/voice/{id}/token", post(voice::token))
        .route("/v1/voice/{id}/participants", get(voice::participants))
        .route("/v1/voice/{id}/live-file", post(voice::live_file))
        .route(
            "/v1/channels/{id}/live-file/token/{nonce}",
            post(voice::data_token),
        )
        .route("/v1/channels/{id}/live-file", post(voice::data_accept))
        .route("/v1/dms", post(dms::create))
        .route(
            "/v1/channels/{id}/message-devices/{device}/{person}",
            get(messages::device),
        )
        .route(
            "/v1/channels/{id}/messages/{message}",
            get(messages::get).delete(messages::delete),
        )
        .route(
            "/v1/channels",
            get(channels::list_channels).post(channels::create_channel),
        )
        .route(
            "/v1/channels/{id}",
            put(channels::update_channel).delete(channels::delete_channel),
        )
        .route(
            "/v1/channels/{id}/permissions/{role}",
            put(channels::permissions),
        )
        .route(
            "/v1/categories",
            get(channels::list_categories).post(channels::create_category),
        )
        .route(
            "/v1/categories/{id}",
            put(channels::update_category).delete(channels::delete_category),
        )
        .route("/v1/me/revocations", post(auth::revoke))
        .route("/v1/invites", post(members::invite))
        .route("/v1/invites/{id}", delete(members::revoke_invite))
        .route("/v1/members/{person}", delete(members::kick))
        .route("/v1/members/{person}/roles", put(roles::assign))
        .route(
            "/v1/bans/{person}",
            put(members::ban).delete(members::unban),
        )
        .route("/v1/roles", get(roles::list).post(roles::create))
        .route("/v1/roles/default", put(roles::set_default))
        .route("/v1/roles/{id}", delete(roles::delete))
        .route("/v1/roles/{id}/name", put(roles::rename))
        .route("/v1/owner", put(roles::transfer))
        .route("/v1/bans", get(members::bans))
        .route("/v1/audit", get(members::audit_log))
        .route("/v1/files/{id}", get(files::download).delete(files::delete))
        .layer(RequestBodyLimitLayer::new(4096))
        .merge(
            Router::new()
                .route(
                    "/v1/settings/public-listing",
                    get(index::get).put(index::update),
                )
                .layer(RequestBodyLimitLayer::new(rusttalk_core::index::BODY_LIMIT)),
        )
        .route("/v1/files", post(files::upload))
        .route("/internal/livekit", post(voice::webhook))
        .merge(
            Router::new()
                .route(rusttalk_core::sync::PATH, get(sync::get).put(sync::put))
                .layer(axum::extract::DefaultBodyLimit::max(
                    rusttalk_core::sync::MAX_BODY,
                )),
        )
        .merge(
            Router::new()
                .route("/v1/space", put(metadata::update_space))
                .route("/v1/me/profile", put(metadata::update_profile))
                .layer(RequestBodyLimitLayer::new(metadata::IMAGE_BODY_LIMIT)),
        )
        .merge(
            Router::new()
                .route(
                    "/v1/channels/{id}/messages",
                    get(messages::list).post(messages::create),
                )
                .layer(RequestBodyLimitLayer::new(messages::BODY_LIMIT)),
        )
        .with_state(app)
        .layer(middleware::from_fn(log_request))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(middleware::map_request(|mut request: Request| async {
            request.headers_mut().remove("x-request-id");
            request
        }));

    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!("API listening on port 8080");
    let result = axum::serve(listener, app).await;
    expiry.abort();
    publisher.abort();
    file_expiry.abort();
    result
}

async fn log_request(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|id| id.to_str().ok());
    let span = tracing::info_span!("request", request_id);
    async move {
        let mut response = next.run(request).await;
        response.headers_mut().insert(
            CACHE_CONTROL,
            "no-store".parse().expect("static Cache-Control value"),
        );
        let status = response.status();
        if status.is_client_error() || status.is_server_error() {
            tracing::info!(
                status = status.as_u16(),
                reason = status.canonical_reason(),
                "request completed"
            );
        } else {
            tracing::info!(status = status.as_u16(), "request completed");
        }
        response
    }
    .instrument(span)
    .await
}
