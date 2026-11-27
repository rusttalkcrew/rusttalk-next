//! Reports from people browsing the directory, and the operator's delist, block and
//! restore. The admin API is off unless `RUSTTALK_INDEX_ADMIN_TOKEN` is set.

use std::net::SocketAddr;

use axum::{
    Json,
    extract::{ConnectInfo, Path, State, rejection::JsonRejection},
    http::{
        HeaderMap, HeaderName, StatusCode,
        header::{AUTHORIZATION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, REFERRER_POLICY},
    },
    response::IntoResponse,
};
use rusttalk_core::{
    auth::decode,
    index::{hidden_format, https_url},
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, query, query_as, query_scalar};

use crate::{App, IndexError, IndexResult, client_ip, clock};

const REASONS: [&str; 5] = ["spam", "scam", "harassment", "illegal", "other"];
const MAX_REPORTS: i64 = 10_000;
const NOTE_LIMIT: usize = 500;
const REASON_LIMIT: usize = 500;

/// Text shown as is to the operator or a server's owner: bounded, without control or
/// hidden characters (`lines` allows line breaks and tabs).
pub(crate) fn plain_text(text: &str, limit: usize, lines: bool) -> bool {
    text.chars().count() <= limit
        && !text.chars().any(|ch| {
            (ch.is_control() && !(lines && matches!(ch, '\n' | '\r' | '\t'))) || hidden_format(ch)
        })
}

fn host(address: &str) -> Option<String> {
    https_url(address)?.host_str().map(str::to_ascii_lowercase)
}

fn server_id(id: &str) -> IndexResult<()> {
    decode::<32>(id)
        .map(drop)
        .map_err(|_| IndexError(StatusCode::BAD_REQUEST, "invalid_server_id"))
}

/// The operator's reason when this server, or any server at its host, was removed.
pub(crate) async fn refused(
    db: &SqlitePool,
    server_id: &str,
    address: &str,
) -> Result<Option<String>, sqlx::Error> {
    query_scalar("SELECT reason FROM sanctions WHERE server_id = ? OR host = ? LIMIT 1")
        .bind(server_id)
        .bind(host(address))
        .fetch_optional(db)
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    pub(crate) reason: String,
    #[serde(default)]
    pub(crate) note: String,
}

pub(crate) async fn report(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<Report>, JsonRejection>,
) -> IndexResult<StatusCode> {
    let ip = client_ip(&app, peer, &headers)?;
    if !app
        .report_limit
        .lock()
        .expect("report limit lock")
        .allow(ip)
    {
        return Err(IndexError(StatusCode::TOO_MANY_REQUESTS, "report_limit"));
    }
    let Json(report) = body.map_err(|_| IndexError(StatusCode::BAD_REQUEST, "invalid_report"))?;
    save_report(&app.db, &id, report, clock()?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Keep a report on a listed server. Nothing about who sent it is stored.
pub(crate) async fn save_report(
    db: &SqlitePool,
    id: &str,
    Report { reason, note }: Report,
    now: i64,
) -> IndexResult<()> {
    server_id(id)?;
    let note = note.trim();
    if !REASONS.contains(&reason.as_str()) || !plain_text(note, NOTE_LIMIT, true) {
        return Err(IndexError(StatusCode::BAD_REQUEST, "invalid_report"));
    }
    // Only listed servers can be reported, and the table stays bounded.
    let saved = query(
        "INSERT INTO reports (server_id, name, address, reason, note, created_at)
         SELECT server_id, name, address, ?2, ?3, ?4 FROM listings
         WHERE server_id = ?1 AND (SELECT COUNT(*) FROM reports) < ?5",
    )
    .bind(id)
    .bind(&reason)
    .bind(note)
    .bind(now)
    .bind(MAX_REPORTS)
    .execute(db)
    .await?;
    if saved.rows_affected() == 0 {
        let listed: bool =
            query_scalar("SELECT EXISTS (SELECT 1 FROM listings WHERE server_id = ?)")
                .bind(id)
                .fetch_one(db)
                .await?;
        return Err(if listed {
            IndexError(StatusCode::SERVICE_UNAVAILABLE, "reports_full")
        } else {
            IndexError(StatusCode::NOT_FOUND, "unknown_server")
        });
    }
    tracing::info!(reason, "server reported");
    Ok(())
}

/// Only the operator, with the configured token.
fn admin(app: &App, headers: &HeaderMap) -> IndexResult<()> {
    let Some(token) = &app.admin_token else {
        return Err(IndexError(StatusCode::NOT_FOUND, "admin_disabled"));
    };
    let given = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !same(given.as_bytes(), token.as_bytes()) {
        return Err(IndexError(StatusCode::UNAUTHORIZED, "admin_token_required"));
    }
    Ok(())
}

/// Compare without stopping at the first difference, so timing does not reveal the token.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

const PAGE_HEADERS: [(HeaderName, &str); 2] = [
    (
        CONTENT_SECURITY_POLICY,
        "default-src 'none'; script-src 'self'; connect-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'",
    ),
    (REFERRER_POLICY, "no-referrer"),
];

/// The operator's page: it asks for the token and uses the API below.
pub(crate) async fn page(State(app): State<App>) -> IndexResult<impl IntoResponse> {
    static_file(&app, "text/html; charset=utf-8", include_str!("admin.html"))
}

pub(crate) async fn script(State(app): State<App>) -> IndexResult<impl IntoResponse> {
    static_file(
        &app,
        "text/javascript; charset=utf-8",
        include_str!("admin.js"),
    )
}

fn static_file(
    app: &App,
    kind: &'static str,
    body: &'static str,
) -> IndexResult<impl IntoResponse + use<>> {
    app.admin_token
        .as_ref()
        .ok_or(IndexError(StatusCode::NOT_FOUND, "admin_disabled"))?;
    Ok((PAGE_HEADERS, [(CONTENT_TYPE, kind)], body))
}

#[derive(Debug, FromRow, Serialize)]
pub(crate) struct ReportRow {
    id: i64,
    pub(crate) server_id: String,
    name: String,
    address: String,
    pub(crate) reason: String,
    pub(crate) note: String,
    created_at: i64,
}

#[derive(Debug, FromRow, Serialize)]
pub(crate) struct SanctionRow {
    pub(crate) server_id: String,
    pub(crate) action: String,
    pub(crate) host: Option<String>,
    pub(crate) reason: String,
    created_at: i64,
}

#[derive(Serialize)]
pub(crate) struct Overview {
    pub(crate) reports: Vec<ReportRow>,
    pub(crate) sanctions: Vec<SanctionRow>,
}

pub(crate) async fn reports(
    State(app): State<App>,
    headers: HeaderMap,
) -> IndexResult<Json<Overview>> {
    admin(&app, &headers)?;
    Ok(Json(overview(&app.db).await?))
}

pub(crate) async fn overview(db: &SqlitePool) -> IndexResult<Overview> {
    let reports = query_as(
        "SELECT id, server_id, name, address, reason, note, created_at
         FROM reports ORDER BY id DESC LIMIT 500",
    )
    .fetch_all(db)
    .await?;
    let sanctions = query_as(
        "SELECT server_id, action, host, reason, created_at FROM sanctions
         ORDER BY created_at DESC",
    )
    .fetch_all(db)
    .await?;
    Ok(Overview { reports, sanctions })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sanction {
    pub(crate) action: String,
    pub(crate) reason: String,
    /// For a block on a server no longer listed or reported; otherwise its last known
    /// address is used.
    #[serde(default)]
    pub(crate) address: Option<String>,
}

pub(crate) async fn sanction(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<Sanction>, JsonRejection>,
) -> IndexResult<StatusCode> {
    admin(&app, &headers)?;
    let Json(sanction) =
        body.map_err(|_| IndexError(StatusCode::BAD_REQUEST, "invalid_sanction"))?;
    place(&app.db, &id, sanction, clock()?).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a server now and refuse it from then on; a block also refuses its host.
/// Its reports are settled with it.
pub(crate) async fn place(
    db: &SqlitePool,
    id: &str,
    Sanction {
        action,
        reason,
        address,
    }: Sanction,
    now: i64,
) -> IndexResult<()> {
    server_id(id)?;
    let reason = reason.trim();
    if !matches!(action.as_str(), "delist" | "block")
        || reason.is_empty()
        || !plain_text(reason, REASON_LIMIT, false)
    {
        return Err(IndexError(StatusCode::BAD_REQUEST, "invalid_sanction"));
    }
    let mut tx = db.begin().await?;
    let host = if action == "block" {
        let address = match address {
            Some(address) => Some(address),
            None => {
                query_scalar(
                    "SELECT address FROM listings WHERE server_id = ?1
                     UNION ALL SELECT * FROM (SELECT address FROM reports
                         WHERE server_id = ?1 ORDER BY id DESC LIMIT 1)",
                )
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
            }
        };
        Some(
            address
                .as_deref()
                .and_then(host)
                .ok_or(IndexError(StatusCode::BAD_REQUEST, "address_required"))?,
        )
    } else {
        None
    };
    query(
        "INSERT INTO sanctions (server_id, action, host, reason, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (server_id) DO UPDATE SET action = excluded.action, host = excluded.host,
             reason = excluded.reason, created_at = excluded.created_at",
    )
    .bind(id)
    .bind(&action)
    .bind(&host)
    .bind(reason)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let mut gone = vec![id.to_owned()];
    if let Some(host) = &host {
        // LIKE narrows it down (ignoring case, as hosts do); the parsed host decides.
        let pattern = format!("https://{}%", host.replace(['%', '_', '\\'], ""));
        let listed: Vec<(String, String)> =
            query_as("SELECT server_id, address FROM listings WHERE address LIKE ?")
                .bind(pattern)
                .fetch_all(&mut *tx)
                .await?;
        gone.extend(
            listed
                .into_iter()
                .filter(|(_, address)| self::host(address).as_ref() == Some(host))
                .map(|(id, _)| id),
        );
    }
    for id in &gone {
        query("DELETE FROM listings WHERE server_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    query("DELETE FROM reports WHERE server_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(
        action,
        removed = gone.len(),
        "server removed from the directory"
    );
    Ok(())
}

pub(crate) async fn restore(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> IndexResult<StatusCode> {
    admin(&app, &headers)?;
    lift(&app.db, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Let a removed server list itself again; it reappears with its next refresh.
pub(crate) async fn lift(db: &SqlitePool, id: &str) -> IndexResult<()> {
    server_id(id)?;
    let lifted = query("DELETE FROM sanctions WHERE server_id = ?")
        .bind(id)
        .execute(db)
        .await?;
    if lifted.rows_affected() == 0 {
        return Err(IndexError(StatusCode::NOT_FOUND, "not_removed"));
    }
    tracing::info!("server restored to the directory");
    Ok(())
}

/// Close a server's reports without acting on them.
pub(crate) async fn dismiss(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> IndexResult<StatusCode> {
    admin(&app, &headers)?;
    server_id(&id)?;
    query("DELETE FROM reports WHERE server_id = ?")
        .bind(&id)
        .execute(&app.db)
        .await?;
    tracing::info!("reports dismissed");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(!same(b"", b"a"));
    }

    #[test]
    fn plain_text_refuses_hidden_and_control_characters() {
        assert!(plain_text("Spam\nlinks", 20, true));
        assert!(!plain_text("Spam\nlinks", 20, false));
        assert!(!plain_text("Clo\u{200B}ver", 20, true));
        assert!(!plain_text("\u{7}", 20, true));
        assert!(!plain_text(&"a".repeat(21), 20, true));
    }
}
