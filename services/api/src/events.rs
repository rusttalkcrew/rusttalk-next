use std::{collections::HashSet, time::Duration};

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket, close_code},
    },
    http::{HeaderMap, Uri},
    response::Response,
};
use rusttalk_core::keys::{Event, KeyNeed, Scope};
use sqlx::{SqliteConnection, query_scalar};
use tokio::{
    sync::broadcast,
    time::{sleep, timeout},
};
use tracing::Instrument;

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate},
    keys,
    store::{now, write},
};

pub async fn upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    if uri.query().is_some() {
        return Err(ApiError::bad_request("invalid_event_query"));
    }
    let span = tracing::Span::current();
    let changes = app.changes.subscribe();
    let (tx, actor) = authenticate(&app, &headers).await?;
    tx.commit().await?;
    Ok(ws
        .max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| serve(app, socket, actor, changes).instrument(span)))
}

async fn serve(
    app: App,
    mut socket: WebSocket,
    actor: Actor,
    mut changes: broadcast::Receiver<Option<Event>>,
) {
    let result = stream(&app, &mut socket, &actor, &mut changes).await;
    let reason = result.unwrap_or_else(|error| error.code);
    tracing::info!(reason, "event socket closed");
    let _ = timeout(
        Duration::from_secs(1),
        socket.send(Message::Close(Some(CloseFrame {
            code: close_code::NORMAL,
            reason: reason.into(),
        }))),
    )
    .await;
}

async fn stream(
    app: &App,
    socket: &mut WebSocket,
    actor: &Actor,
    changes: &mut broadcast::Receiver<Option<Event>>,
) -> ApiResult<&'static str> {
    let expires = sleep(Duration::from_secs(
        (actor.expires_at - now()?).max(0) as u64
    ));
    tokio::pin!(expires);
    let mut pending: HashSet<KeyNeed> = HashSet::new();
    let mut rotations: HashSet<(Scope, i64)> = HashSet::new();
    let mut event = None;
    loop {
        let mut tx = write(&app.db).await?;
        if let Some(reason) = session_end(&mut tx, actor).await? {
            return Ok(reason);
        }
        let next_pending: HashSet<_> = keys::pending(&mut tx, actor).await?.into_iter().collect();
        let next_rotations: HashSet<_> = keys::scopes(&mut tx, actor)
            .await?
            .into_iter()
            .filter(|state| state.rotate)
            .map(|state| (state.scope, state.epoch))
            .collect();
        tx.commit().await?;

        if let Some(notice) = event.take() {
            // A completed wrap is only of interest to its recipient.
            if !matches!(&notice, Event::KeysNeeded(need) if need.device != rusttalk_core::auth::encode(&actor.device))
            {
                send(socket, notice).await?;
            }
        }
        for need in next_pending.difference(&pending) {
            send(socket, Event::KeysNeeded(need.clone())).await?;
        }
        for &(scope, _) in next_rotations.difference(&rotations) {
            send(socket, Event::RotateNeeded { scope }).await?;
        }
        pending = next_pending;
        rotations = next_rotations;

        event = loop {
            tokio::select! {
                _ = &mut expires => return Ok("token_expired"),
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    None | Some(Ok(Message::Close(_))) => return Ok("client_left"),
                    Some(Ok(_)) => return Ok("unexpected_client_message"),
                    Some(Err(_)) => return Ok("socket_io_error"),
                },
                update = changes.recv() => break match update {
                    Ok(notice) => notice,
                    Err(broadcast::error::RecvError::Lagged(_)) => return Ok("event_lagged"),
                    Err(broadcast::error::RecvError::Closed) => return Ok("server_closed"),
                },
            }
        };
    }
}

async fn send(socket: &mut WebSocket, notice: Event) -> ApiResult<()> {
    let text = serde_json::to_string(&notice).map_err(|_| ApiError::internal("event_encoding"))?;
    timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(text.into())),
    )
    .await
    .map_err(|_| ApiError::internal("event_timeout"))?
    .map_err(|_| ApiError::internal("socket_io_error"))
}

async fn session_end(db: &mut SqliteConnection, actor: &Actor) -> ApiResult<Option<&'static str>> {
    if actor.expires_at <= now()? {
        return Ok(Some("token_expired"));
    }
    let live: bool = query_scalar("SELECT EXISTS (SELECT 1 FROM sessions WHERE token_hash = ?)")
        .bind(actor.token_hash.as_slice())
        .fetch_one(&mut *db)
        .await?;
    if live {
        return Ok(None);
    }
    let reason: String = query_scalar(
        "SELECT CASE
        WHEN EXISTS (SELECT 1 FROM bans WHERE person_id = ?) THEN 'banned'
        WHEN NOT EXISTS (SELECT 1 FROM members WHERE person_id = ?) THEN 'kicked'
        WHEN EXISTS (SELECT 1 FROM revocations WHERE person_id = ? AND device_id = ?) THEN 'revoked'
        ELSE 'session_replaced' END",
    )
    .bind(actor.person.as_slice())
    .bind(actor.person.as_slice())
    .bind(actor.person.as_slice())
    .bind(&actor.device)
    .fetch_one(db)
    .await?;
    Ok(Some(match reason.as_str() {
        "banned" => "banned",
        "kicked" => "kicked",
        "revoked" => "revoked",
        _ => "session_replaced",
    }))
}
