use std::{
    collections::HashMap,
    env,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Query, Request, State, rejection::JsonRejection},
    http::{StatusCode, header::CACHE_CONTROL},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusttalk_core::index::{
    BODY_LIMIT, CLOCK_SKEW_MS, Listing, Registration, RegistrationError, now_ms,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{
    SqlitePool, query, query_as, query_scalar,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use tokio::net::TcpListener;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tracing::{Instrument, Level};
use tracing_subscriber::{filter::Targets, prelude::*};

const MAX_TTL: i64 = 3 * 60 * 60;
const MAX_LISTINGS: i64 = 10_000;

#[derive(Clone)]
struct App {
    db: SqlitePool,
    ttl_ms: i64,
    max_listings: i64,
    trust_proxy: bool,
    rate_limit: Arc<Mutex<RateLimit>>,
}

struct RateLimit {
    started: Instant,
    counts: HashMap<IpAddr, u8>,
}

impl RateLimit {
    /// Forget the counts of a finished minute, so client addresses are not kept longer.
    fn expire(&mut self) {
        if self.started.elapsed() >= Duration::from_secs(60) {
            self.started = Instant::now();
            self.counts.clear();
        }
    }

    fn allow(&mut self, ip: IpAddr) -> bool {
        self.expire();
        let ip = match ip.to_canonical() {
            IpAddr::V6(ip) => Ipv6Addr::from(u128::from(ip) & (u128::MAX << 64)).into(),
            ip => ip,
        };
        if self.counts.len() >= 4096 && !self.counts.contains_key(&ip) {
            return false;
        }
        let count = self.counts.entry(ip).or_default();
        if *count >= 16 {
            return false;
        }
        *count += 1;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IndexError(StatusCode, &'static str);

impl From<sqlx::Error> for IndexError {
    fn from(_: sqlx::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, "database_error")
    }
}

impl IntoResponse for IndexError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::error!(reason = self.1, "request failed");
        } else {
            tracing::warn!(reason = self.1, "request rejected");
        }
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

type IndexResult<T> = Result<T, IndexError>;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::registry()
        .with(
            Targets::new()
                .with_default(Level::WARN)
                .with_target("rusttalk_index", Level::INFO),
        )
        .with(tracing_subscriber::fmt::layer().json())
        .init();
    if let Err(error) = run().await {
        tracing::error!(reason = %error, "index stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run() -> Result<()> {
    let data =
        env::var_os("RUSTTALK_DATA_DIR").map_or_else(|| PathBuf::from("/data"), PathBuf::from);
    let ttl = env::var("RUSTTALK_INDEX_TTL_SECONDS")
        .map_or(Ok(MAX_TTL), |value| value.parse())
        .context("invalid listing TTL")?;
    ensure!((1..=MAX_TTL).contains(&ttl), "invalid listing TTL");
    let max_listings = env::var("RUSTTALK_INDEX_MAX_LISTINGS")
        .map_or(Ok(MAX_LISTINGS), |value| value.parse())
        .context("invalid listing limit")?;
    ensure!(max_listings >= 1, "invalid listing limit");
    let db = open(SqliteConnectOptions::new().filename(data.join("index.sqlite3")))
        .await
        .context("open index database")?;
    let listener = TcpListener::bind("0.0.0.0:8080")
        .await
        .context("bind index listener")?;
    let rate_limit = Arc::new(Mutex::new(RateLimit {
        started: Instant::now(),
        counts: HashMap::new(),
    }));
    tokio::spawn(prune(db.clone(), rate_limit.clone()));
    let app = App {
        db,
        ttl_ms: ttl * 1_000,
        max_listings,
        trust_proxy: env::var("RUSTTALK_INDEX_TRUST_PROXY").as_deref() == Ok("1"),
        rate_limit,
    };
    let router = Router::new()
        .route("/v1/health", get(|| async { "ok\n" }))
        .route(
            "/v1/servers",
            post(register)
                .layer(middleware::from_fn_with_state(
                    app.clone(),
                    limit_registration,
                ))
                .get(search),
        )
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(app)
        .layer(middleware::from_fn(log_request))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(middleware::map_request(|mut request: Request| async {
            request.headers_mut().remove("x-request-id");
            request
        }));
    tracing::info!("index listening on port 8080");
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("serve index")?;
    Ok(())
}

async fn open(options: SqliteConnectOptions) -> Result<SqlitePool> {
    let db = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(
            options
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal),
        )
        .await?;
    sqlx::migrate!().run(&db).await?;
    Ok(db)
}

async fn log_request(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|id| id.to_str().ok());
    let span = tracing::info_span!("request", request_id);
    async move {
        let mut response = next.run(request).await;
        response
            .headers_mut()
            .insert(CACHE_CONTROL, "no-store".parse().expect("static header"));
        let status = response.status();
        tracing::info!(
            status = status.as_u16(),
            reason = status.canonical_reason(),
            "request completed"
        );
        response
    }
    .instrument(span)
    .await
}

fn clock() -> IndexResult<i64> {
    now_ms().ok_or(IndexError(StatusCode::INTERNAL_SERVER_ERROR, "clock_error"))
}

async fn prune(db: SqlitePool, rate_limit: Arc<Mutex<RateLimit>>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        rate_limit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expire();
        if let Err(IndexError(_, reason)) = async { prune_expired(&db, clock()?).await }.await {
            tracing::error!(reason, "pruning expired listings failed");
        }
    }
}

async fn prune_expired(db: &SqlitePool, now: i64) -> IndexResult<()> {
    // A short test TTL may expire while its signature is still fresh.
    query("DELETE FROM listings WHERE expires_at <= ? AND timestamp < ?")
        .bind(now)
        .bind(now - CLOCK_SKEW_MS)
        .execute(db)
        .await?;
    Ok(())
}

async fn limit_registration(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> IndexResult<Response> {
    let ip = if app.trust_proxy {
        let values = request.headers().get_all("x-forwarded-for").iter();
        rusttalk_core::auth::forwarded_ip(values.map(|value| value.as_bytes()))
            .ok_or(IndexError(StatusCode::BAD_REQUEST, "invalid_forwarded_ip"))?
    } else {
        peer.ip()
    };
    if !app.rate_limit.lock().expect("rate limit lock").allow(ip) {
        return Err(IndexError(
            StatusCode::TOO_MANY_REQUESTS,
            "registration_limit",
        ));
    }
    Ok(next.run(request).await)
}

async fn register(
    State(app): State<App>,
    body: Result<Json<Registration>, JsonRejection>,
) -> IndexResult<StatusCode> {
    let Json(record) = body.map_err(|error| {
        let status = error.status();
        match status {
            StatusCode::PAYLOAD_TOO_LARGE => IndexError(status, "body_too_large"),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => IndexError(status, "expected_json"),
            _ => IndexError(StatusCode::BAD_REQUEST, "invalid_json"),
        }
    })?;
    save(&app.db, record, clock()?, app.ttl_ms, app.max_listings).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn save(
    db: &SqlitePool,
    record: Registration,
    now: i64,
    ttl_ms: i64,
    max_listings: i64,
) -> IndexResult<()> {
    record.verify(now).map_err(|error| match error {
        RegistrationError::Fields => IndexError(StatusCode::BAD_REQUEST, "invalid_listing"),
        RegistrationError::Timestamp => IndexError(StatusCode::BAD_REQUEST, "stale_registration"),
        RegistrationError::Signature => IndexError(StatusCode::UNAUTHORIZED, "invalid_signature"),
    })?;
    let Listing {
        server_id,
        name,
        description,
        address,
        member_count,
        open_joining,
    } = record.listing;
    // Refreshes always fit; a new server ID needs room under the row cap, which also
    // counts expired rows until pruning so the table size stays bounded.
    let saved = query(
        "INSERT INTO listings (server_id, name, description, address, member_count, open_joining, timestamp, expires_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
         WHERE EXISTS (SELECT 1 FROM listings WHERE server_id = ?1)
             OR (SELECT COUNT(*) FROM listings) < ?9
         ON CONFLICT (server_id) DO UPDATE SET name = excluded.name, description = excluded.description,
             address = excluded.address, member_count = excluded.member_count, open_joining = excluded.open_joining,
             timestamp = excluded.timestamp, expires_at = excluded.expires_at
         WHERE excluded.timestamp > listings.timestamp",
    )
    .bind(&server_id)
    .bind(name)
    .bind(description)
    .bind(address)
    .bind(member_count)
    .bind(open_joining)
    .bind(record.timestamp)
    .bind(now + ttl_ms)
    .bind(max_listings)
    .execute(db)
    .await?;
    if saved.rows_affected() == 0 {
        let known: bool =
            query_scalar("SELECT EXISTS (SELECT 1 FROM listings WHERE server_id = ?)")
                .bind(&server_id)
                .fetch_one(db)
                .await?;
        return Err(if known {
            IndexError(StatusCode::CONFLICT, "replayed_registration")
        } else {
            IndexError(StatusCode::SERVICE_UNAVAILABLE, "index_full")
        });
    }
    Ok(())
}

#[derive(Deserialize)]
struct Search {
    #[serde(default)]
    q: String,
    page: Option<u32>,
}

async fn search(
    State(app): State<App>,
    search: Result<Query<Search>, axum::extract::rejection::QueryRejection>,
) -> IndexResult<Json<Vec<Listing>>> {
    let invalid_query = IndexError(StatusCode::BAD_REQUEST, "invalid_query");
    let Query(Search { q, page }) = search.map_err(|_| invalid_query)?;
    let page = page.unwrap_or(1);
    if !(1..=1_000).contains(&page) || q.chars().count() > 100 || q.contains('\0') {
        return Err(invalid_query);
    }
    Ok(Json(list(&app.db, &q, page, clock()?).await?))
}

async fn list(db: &SqlitePool, q: &str, page: u32, now: i64) -> IndexResult<Vec<Listing>> {
    let pattern = format!(
        "%{}%",
        q.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let rows: Vec<(String, String, String, String, u32, bool)> = query_as(
        "SELECT server_id, name, description, address, member_count, open_joining
         FROM listings WHERE expires_at > ? AND (name LIKE ? ESCAPE '\\' OR description LIKE ? ESCAPE '\\')
         ORDER BY name COLLATE NOCASE, server_id LIMIT 25 OFFSET ?",
    )
    .bind(now)
    .bind(&pattern)
    .bind(&pattern)
    .bind(i64::from(page - 1) * 25)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(server_id, name, description, address, member_count, open_joining)| Listing {
                server_id,
                name,
                description,
                address,
                member_count,
                open_joining,
            },
        )
        .collect())
}

#[cfg(test)]
mod tests;
