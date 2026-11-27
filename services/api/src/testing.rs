//! Shared setup for handler tests: a fresh server with signed-in people.

use axum::http::HeaderMap;
use rusttalk_core::auth::encode;
use sqlx::query;

use crate::{App, store};

/// A fresh server in `dir`. Person 1 owns it; persons 2.. are members with no role yet.
/// Each person `n` signs in with the token `[n; 32]` (see `as_person`).
pub async fn app(dir: &std::path::Path, people: u8) -> anyhow::Result<App> {
    let db = store::open(dir).await?;
    let app = App {
        emoji: crate::emoji::Emoji::open(dir, &db).await?.into(),
        files: crate::files::Files::open(dir, &db).await?.into(),
        // Off, so handler tests can repeat requests; `limits` tests turn them on.
        limits: crate::limits::Limits::for_tests(None, true).into(),
        db,
        key: std::sync::Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32])),
        changes: tokio::sync::broadcast::channel(crate::CHANGE_BACKLOG).0,
        presence: Default::default(),
        voice: None,
        session_ttl: 3600,
        dm_starts: Default::default(),
        listing_changed: tokio::sync::watch::channel(()).0,
    };
    for byte in 1..=people {
        query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
            .bind(person(byte))
            .execute(&app.db)
            .await?;
        query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, '192.0.2.1', ?, ?)")
            .bind(store::hash(&[byte; 32]).as_slice()).bind(person(byte)).bind(vec![byte])
            .bind(i64::MAX).bind(vec![byte]).execute(&app.db).await?;
    }
    query("UPDATE server_settings SET owner_person_id = ?")
        .bind(person(1))
        .execute(&app.db)
        .await?;
    Ok(app)
}

pub fn person(byte: u8) -> Vec<u8> {
    vec![byte; 32]
}

pub fn as_person(byte: u8) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let token = format!("Bearer {}", encode(&[byte; 32]));
    headers.insert("authorization", token.parse().unwrap());
    headers.insert("x-forwarded-for", "192.0.2.1".parse().unwrap());
    headers
}
