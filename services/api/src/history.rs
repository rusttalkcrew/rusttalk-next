//! How many messages each channel keeps. Someone with Manage Server chooses it in
//! Server settings; the host caps it only when it sets `RUSTTALK_MESSAGE_RETAIN`.

use std::{env, sync::OnceLock};

use anyhow::{Context, Result, ensure};
use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::HeaderMap,
};
use rusttalk_core::{auth::MessageHistory, keys::Event};
use sqlx::{SqliteConnection, query, query_scalar};

use crate::{
    ApiError, ApiResult, App,
    auth::{authenticate, authenticate_read},
    messages, payload,
    roles::{self, Permission},
    store,
};

/// What a channel keeps until someone saves their own number or the host sets one.
pub const DEFAULT: i64 = 10_000;

/// The host's cap (none when unset) and the default it implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Host {
    ceiling: Option<i64>,
    default: i64,
}

static HOST: OnceLock<Host> = OnceLock::new();

/// Read `RUSTTALK_MESSAGE_RETAIN` once at startup, so a bad value stops the server.
pub fn init() -> Result<()> {
    let host = host_from(env::var("RUSTTALK_MESSAGE_RETAIN").ok().as_deref())?;
    HOST.get_or_init(|| host);
    Ok(())
}

fn host() -> Host {
    *HOST.get_or_init(|| Host {
        ceiling: None,
        default: DEFAULT,
    })
}

/// Like the file limits, a set variable is both the cap and the default. 0 keeps every
/// message, which caps nothing; unset or empty (as Compose passes `${NAME:-}`) is no cap.
fn host_from(value: Option<&str>) -> Result<Host> {
    let value = match value.filter(|value| !value.is_empty()) {
        None => {
            return Ok(Host {
                ceiling: None,
                default: DEFAULT,
            });
        }
        Some(value) => value
            .parse::<i64>()
            .context("invalid RUSTTALK_MESSAGE_RETAIN")?,
    };
    ensure!(value >= 0, "RUSTTALK_MESSAGE_RETAIN must be 0 or more");
    Ok(Host {
        ceiling: (value > 0).then_some(value),
        default: value,
    })
}

impl Host {
    /// A saved number (or the default) within the cap; 0 means no limit, so it is above any cap.
    fn apply(self, saved: Option<i64>) -> i64 {
        let keep = saved.unwrap_or(self.default);
        match self.ceiling {
            Some(ceiling) if keep == 0 || keep > ceiling => ceiling,
            _ => keep,
        }
    }
}

/// How many messages each channel keeps now; 0 keeps them all.
pub async fn keep(db: &mut SqliteConnection) -> ApiResult<i64> {
    let saved: Option<i64> =
        query_scalar("SELECT message_history FROM server_settings WHERE id = 1")
            .fetch_one(db)
            .await?;
    Ok(host().apply(saved))
}

fn reply(keep: i64) -> Json<MessageHistory> {
    Json(MessageHistory {
        keep,
        ceiling: host().ceiling,
    })
}

pub async fn get(State(app): State<App>, headers: HeaderMap) -> ApiResult<Json<MessageHistory>> {
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    let keep = keep(&mut tx).await?;
    tx.commit().await?;
    Ok(reply(keep))
}

/// Save a new number. A lower one deletes every channel's older messages right away.
pub async fn set(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<MessageHistory>, JsonRejection>,
) -> ApiResult<Json<MessageHistory>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    roles::authority(&mut tx, &actor.person)
        .await?
        .require(Permission::ManageServer)?;
    let requested = payload(body)?.keep;
    if requested < 0 {
        return Err(ApiError::bad_request("invalid_message_history"));
    }
    let keep = host().apply(Some(requested));
    query("UPDATE server_settings SET message_history = ? WHERE id = 1")
        .bind(keep)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    trim_all(&app).await?;
    Ok(reply(keep))
}

/// Trim every channel to the current limit, one channel per transaction so a large cleanup
/// never holds the writer for long. A channel left over (after a failure) is trimmed by
/// its next message instead.
pub async fn trim_all(app: &App) -> ApiResult<()> {
    loop {
        let mut tx = store::write(&app.db).await?;
        let keep = keep(&mut tx).await?;
        if keep == 0 {
            return Ok(());
        }
        let Some((channel, count)): Option<(String, i64)> = sqlx::query_as(
            "SELECT id, message_count FROM channels WHERE message_count > ? LIMIT 1",
        )
        .bind(keep)
        .fetch_optional(&mut *tx)
        .await?
        else {
            return Ok(());
        };
        let dropped = messages::trim(&mut tx, &channel, count, keep).await?;
        if dropped.is_empty() {
            // The count overstated the rows; correct it so this loop moves on.
            tracing::warn!("message count out of step; recounting");
            query(
                "UPDATE channels SET message_count =
                     (SELECT COUNT(*) FROM messages WHERE channel_id = channels.id) WHERE id = ?",
            )
            .bind(&channel)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        tracing::info!(dropped = dropped.len(), keep, "older messages removed");
        for id in dropped {
            app.notify(Event::MessageDeleted {
                channel: channel.clone(),
                id,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{app, as_person};

    #[test]
    fn the_host_caps_only_when_it_sets_the_variable() -> anyhow::Result<()> {
        let free = host_from(None)?;
        assert_eq!(free, host_from(Some(""))?);
        assert_eq!(free.apply(None), DEFAULT);
        assert_eq!(free.apply(Some(0)), 0);
        assert_eq!(free.apply(Some(1_000_000)), 1_000_000);

        let capped = host_from(Some("500"))?;
        assert_eq!(capped.apply(None), 500);
        assert_eq!(capped.apply(Some(20)), 20);
        assert_eq!(capped.apply(Some(0)), 500);
        assert_eq!(capped.apply(Some(501)), 500);

        // 0 lets owners keep everything, and is also the default then.
        let all = host_from(Some("0"))?;
        assert_eq!((all.ceiling, all.apply(None)), (None, 0));
        assert!(host_from(Some("-1")).is_err());
        assert!(host_from(Some("lots")).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn lowering_the_limit_trims_every_channel_now() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let app = app(dir.path(), 2).await?;
        let mut events = app.changes.subscribe();
        let channels = [("A", 5), ("B", 2), ("C", 0)];
        query("INSERT INTO categories VALUES ('category', 0, X'00')")
            .execute(&app.db)
            .await?;
        query("INSERT INTO message_senders VALUES (X'01', X'02', X'03')")
            .execute(&app.db)
            .await?;
        for (position, (channel, count)) in channels.into_iter().enumerate() {
            query(
                "INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, 'text', 'category', ?, X'00')",
            )
            .bind(channel)
            .bind(position as i64)
            .execute(&app.db)
            .await?;
            sqlx::raw_sql(&format!(
                "INSERT INTO key_scopes (id, kind, channel_id) VALUES ('{channel}', 'channel', '{channel}');
                 INSERT INTO key_epochs (scope_id, epoch, commitment) VALUES ('{channel}', 1, zeroblob(32));"
            ))
            .execute(&app.db)
            .await?;
            for i in 0..count {
                query(
                    "INSERT INTO messages (id, channel_id, epoch, sender_device, sender_person, at, size, envelope)
                     VALUES (?, ?, 1, X'01', X'02', 0, 111, zeroblob(111))",
                )
                .bind(format!("{channel}{i}"))
                .bind(channel)
                .execute(&app.db)
                .await?;
            }
            query("UPDATE channels SET message_count = ? WHERE id = ?")
                .bind(count)
                .bind(channel)
                .execute(&app.db)
                .await?;
        }
        let Json(initial) = get(State(app.clone()), as_person(2)).await?;
        assert_eq!(
            initial,
            MessageHistory {
                keep: DEFAULT,
                ceiling: None
            }
        );

        let save = |keep| {
            set(
                State(app.clone()),
                as_person(1),
                Ok(Json(MessageHistory {
                    keep,
                    ceiling: None,
                })),
            )
        };
        assert_eq!(
            set(
                State(app.clone()),
                as_person(2),
                Ok(Json(MessageHistory {
                    keep: 1,
                    ceiling: None
                }))
            )
            .await
            .unwrap_err()
            .code,
            "permission_denied"
        );
        assert_eq!(save(-1).await.unwrap_err().code, "invalid_message_history");

        let Json(saved) = save(2).await?;
        assert_eq!(saved.keep, 2);
        let left: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT c.id, c.message_count, (SELECT COUNT(*) FROM messages m WHERE m.channel_id = c.id)
             FROM channels c ORDER BY c.id",
        )
        .fetch_all(&app.db)
        .await?;
        assert_eq!(
            left,
            [("A".into(), 2, 2), ("B".into(), 2, 2), ("C".into(), 0, 0)]
        );
        let kept: Vec<String> =
            query_scalar("SELECT id FROM messages WHERE channel_id = 'A' ORDER BY sequence")
                .fetch_all(&app.db)
                .await?;
        assert_eq!(kept, ["A3", "A4"]);
        // Apps learn of each removed message.
        let mut deleted = Vec::new();
        while let Ok(change) = events.try_recv() {
            if let crate::Change::Update(notice) = change
                && let rusttalk_core::presence::Update::Event(Event::MessageDeleted { id, .. }) =
                    &notice.update
            {
                deleted.push(id.clone());
            }
        }
        assert_eq!(deleted, ["A0", "A1", "A2"]);

        // A count that overstates the rows is corrected instead of trimmed forever.
        query("UPDATE channels SET message_count = 9 WHERE id = 'B'")
            .execute(&app.db)
            .await?;
        trim_all(&app).await?;
        let count: i64 = query_scalar("SELECT message_count FROM channels WHERE id = 'B'")
            .fetch_one(&app.db)
            .await?;
        assert_eq!(count, 2);

        // Keeping everything removes nothing.
        let Json(saved) = save(0).await?;
        assert_eq!(saved.keep, 0);
        let Json(read) = get(State(app), as_person(2)).await?;
        assert_eq!(read.keep, 0);
        Ok(())
    }
}
