mod auth;
mod calendar;
mod channels;
mod dms;
mod emoji;
mod events;
mod file_settings;
mod files;
mod history;
mod index;
mod keys;
mod limits;
mod lockdown;
mod members;
mod messages;
mod metadata;
mod new_members;
mod page_files;
mod presence;
mod reports;
mod roles;
mod slow_mode;
mod stats;
mod store;
mod sync;
mod takedown;
#[cfg(test)]
mod testing;
mod timeouts;
mod updates;
mod voice;

use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{MatchedPath, Request, rejection::JsonRejection},
    http::{
        HeaderMap, Method, StatusCode,
        header::{CACHE_CONTROL, CONTENT_LENGTH, RETRY_AFTER},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use ed25519_dalek::SigningKey;
use futures_util::TryStreamExt;
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
    time,
};
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
};
use tracing::{Instrument, Level};
use tracing_subscriber::{filter::Targets, fmt::writer::BoxMakeWriter, prelude::*};

#[derive(Clone)]
struct App {
    db: SqlitePool,
    key: Arc<SigningKey>,
    changes: broadcast::Sender<Change>,
    presence: Arc<presence::Sessions>,
    voice: Option<Arc<voice::LiveKit>>,
    listing_changed: watch::Sender<()>,
    /// Recent join times, for automatic lockdown.
    joins: Arc<lockdown::Joins>,
    files: Arc<files::Files>,
    emoji: Arc<emoji::Emoji>,
    limits: Arc<limits::Limits>,
    session_ttl: i64,
    dm_starts: Arc<dms::Starts>,
    stats: Arc<stats::Stats>,
}

/// Events a socket may fall behind before it must resync. A slot frees once every socket has
/// read it, so the cost is only held for sockets that are actually behind.
const CHANGE_BACKLOG: usize = 1024;

/// What event sockets hear: an update for clients, or a server-side change to recheck.
#[derive(Clone)]
enum Change {
    /// Shared, so each update is cloned and encoded once however many sockets listen.
    Update(Arc<events::Notice>),
    /// This person's sessions were replaced or refreshed, so their sockets recheck their own
    /// session.
    Session(rusttalk_core::identity::PersonId),
    /// A new key epoch or a reopened wrap, so each socket recomputes pending wraps and scopes.
    Keys,
    /// Like `Keys`, but only these people's scopes or wraps changed (a DM's two people), so
    /// only their sockets recheck.
    Access([rusttalk_core::identity::PersonId; 2]),
}

impl App {
    fn notify(&self, event: Event) {
        let _ = self
            .changes
            .send(Change::update(rusttalk_core::presence::Update::Event(
                event,
            )));
    }

    fn signal(&self, change: Change) {
        let _ = self.changes.send(change);
    }
}

type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    /// The underlying error for the server log only, such as SQLite's message.
    detail: Option<String>,
    /// For 429s: the seconds until the request may succeed, sent as `Retry-After` and
    /// `retry_after`.
    retry_after: Option<i64>,
    /// Text for the person refused, sent as `message`: only a lockdown's notice.
    message: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            detail: None,
            retry_after: None,
            message: None,
        }
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
    fn too_many(code: &'static str, retry_after: i64) -> Self {
        Self {
            retry_after: Some(retry_after),
            ..Self::new(StatusCode::TOO_MANY_REQUESTS, code)
        }
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
        // sqlx and SQLite messages name the failing code, constraint or column, never values.
        Self {
            detail: Some(error.to_string()),
            ..Self::internal(code)
        }
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
            tracing::error!(reason = self.code, error = self.detail, "request failed");
        } else if self.retry_after.is_none() {
            // Rate limits log once a minute per person themselves, not every refusal.
            tracing::warn!(reason = self.code, "request rejected");
        }
        let mut body = json!({ "error": self.code });
        if let Some(secs) = self.retry_after {
            body["retry_after"] = secs.into();
        }
        if let Some(message) = self.message {
            body["message"] = message.into();
        }
        match self.retry_after {
            Some(secs) => {
                (self.status, [(RETRY_AFTER, secs.to_string())], Json(body)).into_response()
            }
            None => (self.status, Json(body)).into_response(),
        }
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
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(log_writer()),
        )
        .init();
    if let Err(error) = run().await {
        tracing::error!(error = %format_args!("{error:#}"), "API command failed");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Server logs go to stdout. A CLI command prints its value there, so its logs (such as
/// sqlx's slow-statement warning while the server holds the writer) go to stderr instead.
fn log_writer() -> BoxMakeWriter {
    if env::args_os().nth(1).is_some() {
        BoxMakeWriter::new(io::stderr)
    } else {
        BoxMakeWriter::new(io::stdout)
    }
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
            history::init()?;
            let files = Arc::new(files::Files::open(&dir, &db).await?);
            let emoji = Arc::new(emoji::Emoji::open(&dir, &db).await?);
            let limits = Arc::new(limits::Limits::open(&mut *db.acquire().await?).await?);
            serve(App {
                files,
                emoji,
                limits,
                db,
                key: Arc::new(key),
                changes: broadcast::channel(CHANGE_BACKLOG).0,
                presence: Arc::default(),
                listing_changed: watch::channel(()).0,
                joins: Default::default(),
                voice: voice::LiveKit::from_env()?.map(Arc::new),
                session_ttl: auth::session_ttl()?,
                dm_starts: Arc::new(dms::Starts::from_env()?),
                stats: Arc::new(stats::Stats::new(&dir)),
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
    let expiry = tokio::spawn(store::expire_records(app.clone()));
    // Apply mutes and access changes that a restart or LiveKit outage left unapplied.
    voice::start_enforcer(&app);
    // Give voice back when the timeouts running now end.
    if let Err(error) = timeouts::resume(&app).await {
        tracing::error!(reason = error.code, "scheduling timeout ends failed");
    }
    let sampler = tokio::spawn(stats::sample(app.stats.clone()));
    let recorder = tokio::spawn(stats::history::record(app.clone()));
    let publisher = tokio::spawn(index::refresh(app.clone(), app.listing_changed.subscribe()));
    let file_expiry = tokio::spawn(files::expire(app.clone()));
    // A host that lowered RUSTTALK_MESSAGE_RETAIN gets its channels trimmed now.
    let trim = app.clone();
    tokio::spawn(async move {
        if let Err(error) = history::trim_all(&trim).await {
            tracing::error!(reason = error.code, "trimming message history failed");
        }
    });
    let stats = app.stats.clone();
    let app = routes(app)
        .layer(middleware::from_fn(finish_changes))
        .layer(middleware::from_fn(log_request))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(middleware::map_request(|mut request: Request| async {
            request.headers_mut().remove("x-request-id");
            request
        }));

    let listener = stats::Counted {
        listener: TcpListener::bind("0.0.0.0:8080").await?,
        stats,
    };
    tracing::info!("API listening on port 8080");
    let result = axum::serve(listener, app).await;
    expiry.abort();
    sampler.abort();
    recorder.abort();
    publisher.abort();
    file_expiry.abort();
    result
}

fn routes(app: App) -> Router {
    Router::new()
        .route("/v1/health", get(|| async { "ok\n" }))
        .route("/v1/server-info", get(members::server_info))
        .route("/v1/settings/updates", get(updates::status))
        .route(
            "/v1/settings/files",
            get(file_settings::get).put(file_settings::set),
        )
        .route(
            "/v1/settings/rate-limits",
            get(limits::get).put(limits::set),
        )
        .route(
            "/v1/settings/message-history",
            get(history::get).put(history::set),
        )
        .route(
            "/v1/settings/new-member-rules",
            get(new_members::get).put(new_members::set),
        )
        .route(
            "/v1/settings/open-joining",
            get(members::open_joining).put(members::set_open_joining),
        )
        .route(
            "/v1/settings/lockdown",
            get(lockdown::get).put(lockdown::set),
        )
        .route("/v1/auth/join", post(auth::open_join))
        .route("/v1/auth/challenge", get(auth::challenge))
        .route("/v1/auth/sign-in", post(auth::sign_in))
        .route("/v1/auth/refresh", post(auth::refresh))
        .route("/v1/me", get(members::me))
        .route("/v1/me/status", put(presence::set))
        .route("/v1/me/idle", put(presence::idle))
        .route("/v1/me/hide-offline", put(presence::set_hide_offline))
        .route("/v1/me/membership", delete(members::leave))
        .route("/v1/space", get(metadata::space))
        .route("/v1/space/icon", get(metadata::icon))
        .route("/v1/members", get(metadata::members))
        .route("/v1/members/{person}/avatar", get(metadata::avatar))
        .route("/v1/events", get(events::upgrade))
        .route("/v1/keys", get(keys::state).post(keys::upload))
        .route("/v1/keys/epochs", post(keys::new_epoch))
        .route("/v1/keys/reject", post(keys::reject))
        .route("/v1/member-devices", get(keys::devices))
        .route("/v1/invite-preview", post(members::preview))
        .route("/v1/catalog", get(channels::catalog))
        .route("/v1/calendar/{id}", delete(calendar::remove))
        .route("/v1/calendar/{id}/approve", post(calendar::approve))
        .route("/v1/calendar/{id}/answer", put(calendar::answer))
        .route("/v1/calendar/{id}/remind", post(calendar::remind))
        .route("/v1/settings/calendar", put(calendar::set_enabled))
        .route("/v1/voice/{id}/token", post(voice::token))
        .route("/v1/voice/{id}/participants", get(voice::participants))
        .route(
            "/v1/voice/{id}/participants/{person}",
            delete(voice::moderation::remove),
        )
        .route(
            "/v1/voice/{id}/participants/{person}/move",
            post(voice::moderation::move_to),
        )
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
        .route("/v1/channels/{id}/focus", put(channels::focus))
        .route(
            "/v1/categories",
            get(channels::list_categories).post(channels::create_category),
        )
        .route(
            "/v1/categories/{id}",
            put(channels::update_category).delete(channels::delete_category),
        )
        .route("/v1/me/revocations", post(auth::revoke))
        .route("/v1/invites", get(members::invites).post(members::invite))
        .route("/v1/invites/{id}", delete(members::revoke_invite))
        .route("/v1/members/{person}", delete(members::kick))
        .route("/v1/members/{person}/roles", put(roles::assign))
        .route(
            "/v1/members/{person}/voice-mute",
            put(voice::moderation::mute).delete(voice::moderation::unmute),
        )
        .route(
            "/v1/members/{person}/timeout",
            put(timeouts::set).delete(timeouts::end),
        )
        .route(
            "/v1/bans/{person}",
            put(members::ban).delete(members::unban),
        )
        .route("/v1/roles", get(roles::list).post(roles::create))
        .route("/v1/roles/default", put(roles::set_default))
        .route("/v1/roles/{id}", put(roles::edit).delete(roles::delete))
        .route("/v1/roles/{id}/name", put(roles::rename))
        .route("/v1/owner", put(roles::transfer))
        .route("/v1/bans", get(members::bans))
        .route("/v1/reports/reviewers", get(reports::list_reviewers))
        .route("/v1/reports/{id}/resolve", post(reports::resolve))
        .route(
            "/v1/reports/from/{person}/dismiss",
            post(reports::dismiss_from),
        )
        .route("/v1/audit", get(members::audit_log))
        .route("/v1/stats", get(stats::show))
        .route("/v1/stats/history", get(stats::history))
        .route("/v1/files/{id}", get(files::download).delete(files::delete))
        .route("/v1/emoji", get(emoji::list))
        .route("/v1/emoji/packs", post(emoji::add_pack))
        .route("/v1/emoji/packs/order", put(emoji::order_packs))
        .route("/v1/emoji/packs/{id}", delete(emoji::delete_pack))
        .route("/v1/emoji/packs/{id}/name", put(emoji::rename_pack))
        .route("/v1/emoji/{id}", delete(emoji::delete))
        .route("/v1/emoji/{id}/pack", put(emoji::move_to_pack))
        .route("/v1/emoji/{id}/image", get(emoji::image))
        .route("/v1/emoji/{id}/name", put(emoji::rename))
        .layer(RequestBodyLimitLayer::new(4096))
        .merge(
            Router::new()
                .route("/v1/takedowns", get(takedown::list).post(takedown::create))
                .layer(RequestBodyLimitLayer::new(takedown::BODY_LIMIT)),
        )
        .merge(
            Router::new()
                .route(
                    "/v1/settings/public-listing",
                    get(index::get).put(index::update),
                )
                .layer(RequestBodyLimitLayer::new(rusttalk_core::index::BODY_LIMIT)),
        )
        .merge(
            Router::new()
                .route("/v1/calendar", get(calendar::list).post(calendar::create))
                .layer(RequestBodyLimitLayer::new(
                    rusttalk_core::calendar::BODY_LIMIT,
                )),
        )
        .merge(
            Router::new()
                .route("/v1/channels/order", put(channels::reorder))
                .layer(RequestBodyLimitLayer::new(channels::ORDER_BODY_LIMIT)),
        )
        .merge(
            Router::new()
                .route(
                    "/v1/channels/{id}/files",
                    get(files::shared).post(files::share),
                )
                .layer(RequestBodyLimitLayer::new(files::SHARE_BODY_LIMIT)),
        )
        .route("/v1/files", post(files::upload))
        .route("/internal/livekit", post(voice::webhook))
        .route(rusttalk_core::sync::PATH, get(sync::get).put(sync::put))
        .merge(
            Router::new()
                .route("/v1/space", put(metadata::update_space))
                .route("/v1/me/profile", put(metadata::update_profile))
                .route("/v1/emoji", post(emoji::add))
                .layer(RequestBodyLimitLayer::new(metadata::IMAGE_BODY_LIMIT)),
        )
        .merge(
            Router::new()
                .route("/v1/reports", get(reports::list).post(reports::create))
                .layer(RequestBodyLimitLayer::new(
                    rusttalk_core::reports::BODY_LIMIT,
                )),
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
}

/// Changes running at once. Each keeps running after its client leaves, so this bounds what
/// clients that send and hang up can leave behind during a slow write.
const CHANGE_LIMIT: usize = 256;
static CHANGES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(CHANGE_LIMIT);

/// How long a change's body may take to arrive. It is read before the change takes a slot,
/// so a slow or stalled sender only ties up its own connection.
const BODY_READ_LIMIT: Duration = Duration::from_secs(30);

const fn max(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
}

/// The largest body any buffered route accepts; each route still applies its own limit.
const BUFFERED_BODY_LIMIT: usize = max(
    max(
        max(4096, rusttalk_core::index::BODY_LIMIT),
        max(
            rusttalk_core::calendar::BODY_LIMIT,
            channels::ORDER_BODY_LIMIT,
        ),
    ),
    max(
        max(files::SHARE_BODY_LIMIT, rusttalk_core::sync::MAX_BODY),
        max(
            max(metadata::IMAGE_BODY_LIMIT, messages::BODY_LIMIT),
            rusttalk_core::reports::BODY_LIMIT,
        ),
    ),
);

/// File uploads stream to disk with their own idle and total limits, and read their body
/// only while holding one of the few upload slots.
const STREAMED_ROUTES: &[&str] = &["/v1/files"];

/// Run every change to completion even if the client leaves. Otherwise a cancelled request
/// can commit and then skip what follows, such as notifying members or voice enforcement.
async fn finish_changes(request: Request, next: Next) -> Response {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return next.run(request).await;
    }
    let streamed = request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|path| STREAMED_ROUTES.contains(&path.as_str()));
    // Read the body first, so a slow sender cannot hold a slot that every change shares.
    let request = if streamed {
        request
    } else {
        let (parts, body) = request.into_parts();
        match buffer_body(&parts.headers, body).await {
            Ok(body) => Request::from_parts(parts, body),
            Err(error) => return error.into_response(),
        }
    };
    let Ok(permit) = CHANGES.try_acquire() else {
        return ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "server_busy").into_response();
    };
    // The task owns the permit, so a client that leaves cannot free its slot early.
    tokio::spawn(
        async move {
            let response = next.run(request).await;
            drop(permit);
            response
        }
        .in_current_span(),
    )
    .await
    .unwrap_or_else(|_| ApiError::internal("request_task_failed").into_response())
}

/// Reads a change's whole body within `BODY_READ_LIMIT`. A larger body is read to its end
/// (within the same time) before the 413, so the proxy does not report a reset stream as 502.
async fn buffer_body(headers: &HeaderMap, body: Body) -> ApiResult<Body> {
    let mut oversized = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > BUFFERED_BODY_LIMIT as u64);
    let mut bytes = Vec::new();
    let read = time::timeout(BODY_READ_LIMIT, async {
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.try_next().await? {
            if oversized || chunk.len() > BUFFERED_BODY_LIMIT - bytes.len() {
                oversized = true;
                continue;
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok::<_, axum::Error>(())
    })
    .await;
    if oversized {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
        ));
    }
    match read {
        Err(_) => Err(ApiError::new(StatusCode::REQUEST_TIMEOUT, "body_timeout")),
        Ok(Err(_)) => Err(ApiError::bad_request("invalid_body")),
        Ok(Ok(())) => Ok(Body::from(bytes)),
    }
}

async fn log_request(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|id| id.to_str().ok());
    // The route pattern, not the path, so logs never carry IDs from the URL.
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str);
    let method = request.method().as_str();
    let span = tracing::info_span!("request", request_id, method, route);
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

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use futures_util::StreamExt;
    use sqlx::query_scalar;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        time::{sleep, timeout},
    };

    use super::*;
    use crate::testing;

    /// These tests count `CHANGES` permits, so they run one at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn serve_kicks(app: &App) -> anyhow::Result<SocketAddr> {
        let router = Router::new()
            .route("/v1/members/{person}", delete(members::kick))
            .with_state(app.clone())
            .layer(middleware::from_fn(finish_changes));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(address)
    }

    /// Send a kick of person 2 with this bearer token and return the open connection.
    async fn send_kick(address: SocketAddr, token: [u8; 32]) -> anyhow::Result<TcpStream> {
        let mut client = TcpStream::connect(address).await?;
        let request = format!(
            "DELETE /v1/members/{} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {}\r\n\
             X-Forwarded-For: 192.0.2.1\r\nContent-Length: 0\r\n\r\n",
            encode(&testing::person(2)),
            encode(&token),
        );
        client.write_all(request.as_bytes()).await?;
        Ok(client)
    }

    /// The response status if one arrives within `wait`, then hang up either way.
    async fn status_within(mut client: TcpStream, wait: Duration) -> Option<u16> {
        let mut head = [0; 12];
        timeout(wait, client.read_exact(&mut head))
            .await
            .ok()?
            .ok()?;
        std::str::from_utf8(&head[9..12]).ok()?.parse().ok()
    }

    async fn permits_reach(available: usize) -> anyhow::Result<()> {
        // Generous: the writer turn is shared with other tests, such as the 6 s slow write.
        timeout(Duration::from_secs(30), async {
            while CHANGES.available_permits() != available {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }

    /// A change route that reads its whole body, like the JSON routes.
    async fn serve_echo() -> anyhow::Result<SocketAddr> {
        let router = Router::new()
            .route("/echo", post(|body: String| async move { body }))
            .layer(middleware::from_fn(finish_changes));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move { axum::serve(listener, router).await });
        Ok(address)
    }

    /// Announce `length` body bytes but send only `sent` of them.
    async fn send_echo(
        address: SocketAddr,
        length: usize,
        sent: &str,
    ) -> anyhow::Result<TcpStream> {
        let mut client = TcpStream::connect(address).await?;
        let request =
            format!("POST /echo HTTP/1.1\r\nHost: test\r\nContent-Length: {length}\r\n\r\n{sent}");
        client.write_all(request.as_bytes()).await?;
        Ok(client)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_bodies_hold_no_change_slots() -> anyhow::Result<()> {
        let _serial = SERIAL.lock().await;
        let address = serve_echo().await?;
        let mut stalled = Vec::new();
        for _ in 0..CHANGE_LIMIT + 8 {
            stalled.push(send_echo(address, 4000, "x").await?);
        }
        // Let the server read every stalled request's head and first byte.
        sleep(Duration::from_millis(500)).await;
        assert_eq!(CHANGES.available_permits(), CHANGE_LIMIT);

        let client = send_echo(address, 2, "ok").await?;
        assert_eq!(
            status_within(client, Duration::from_secs(5)).await,
            Some(200)
        );
        drop(stalled);
        permits_reach(CHANGE_LIMIT).await
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_body_times_out() {
        let stalled = futures_util::stream::once(async {
            Ok::<_, io::Error>(axum::body::Bytes::from_static(b"x"))
        })
        .chain(futures_util::stream::pending());
        let error = buffer_body(&HeaderMap::new(), Body::from_stream(stalled))
            .await
            .expect_err("a stalled body is refused");
        assert_eq!(error.status, StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn an_oversized_body_is_refused_after_it_ends() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, (BUFFERED_BODY_LIMIT + 1).into());
        let body = Body::from(vec![0; BUFFERED_BODY_LIMIT + 1]);
        let error = buffer_body(&headers, body)
            .await
            .expect_err("an oversized body is refused");
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        let body = buffer_body(&HeaderMap::new(), Body::from("{}")).await;
        assert!(body.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_change_finishes_after_the_client_leaves() -> anyhow::Result<()> {
        let _serial = SERIAL.lock().await;
        let dir = tempfile::tempdir()?;
        let app = testing::app(dir.path(), 2).await?;
        let mut changes = app.changes.subscribe();
        let address = serve_kicks(&app).await?;

        // Holding the writer turn parks the kick mid-request while its client hangs up.
        let turn = store::write(&app.db).await?;
        let client = send_kick(address, [1; 32]).await?;
        permits_reach(CHANGE_LIMIT - 1).await?;
        drop(client);
        // Give the server time to see the hang-up, which used to cancel the handler.
        sleep(Duration::from_millis(200)).await;
        drop(turn);

        timeout(Duration::from_secs(30), async {
            loop {
                if let Change::Update(notice) = changes.recv().await?
                    && let rusttalk_core::presence::Update::Event(Event::MemberChanged { .. }) =
                        notice.update
                {
                    return anyhow::Ok(());
                }
            }
        })
        .await??;
        let member: bool =
            query_scalar("SELECT EXISTS (SELECT 1 FROM members WHERE person_id = ?)")
                .bind(testing::person(2))
                .fetch_one(&app.db)
                .await?;
        assert!(!member, "the kick committed");
        permits_reach(CHANGE_LIMIT).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn changes_left_behind_by_clients_stay_bounded() -> anyhow::Result<()> {
        let _serial = SERIAL.lock().await;
        let dir = tempfile::tempdir()?;
        let app = testing::app(dir.path(), 2).await?;
        let address = serve_kicks(&app).await?;
        let turn = store::write(&app.db).await?;

        // Unknown sessions are refused while the writer turn is taken, without queueing for it.
        for byte in 0..32 {
            let client = send_kick(address, [200 + byte as u8; 32]).await?;
            let status = status_within(client, Duration::from_secs(5)).await;
            assert_eq!(status, Some(401));
        }
        permits_reach(CHANGE_LIMIT).await?;

        // Valid changes that queue and hang up fill the admission slots, then are refused.
        let sent = CHANGE_LIMIT + 64;
        let mut clients = Vec::new();
        for _ in 0..sent {
            clients.push(send_kick(address, [1; 32]).await?);
        }
        let answers: Vec<_> = clients
            .into_iter()
            .map(|client| tokio::spawn(status_within(client, Duration::from_secs(2))))
            .collect();
        let (mut parked, mut busy) = (0, 0);
        for answer in answers {
            match answer.await? {
                None => parked += 1,
                Some(503) => busy += 1,
                other => panic!("unexpected answer {other:?}"),
            }
        }
        assert_eq!(parked, CHANGE_LIMIT);
        assert_eq!(busy, sent - CHANGE_LIMIT);
        assert_eq!(CHANGES.available_permits(), 0);

        // Once the write ends, the left-behind changes finish and free every slot.
        drop(turn);
        permits_reach(CHANGE_LIMIT).await
    }
}
