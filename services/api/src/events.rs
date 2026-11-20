use std::{collections::HashSet, time::Duration};

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket, close_code},
    },
    http::{HeaderMap, Uri},
    response::Response,
};
use rusttalk_core::{
    auth::encode,
    keys::{Event, KeyNeed, Kind, Scope},
    presence::Update,
};
use sqlx::{SqliteConnection, query_scalar};
use tokio::{
    sync::broadcast,
    time::{Instant, sleep, timeout},
};
use tracing::Instrument;

use crate::{
    ApiError, ApiResult, App,
    auth::{Actor, authenticate},
    keys, presence,
    store::now,
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
    mut changes: broadcast::Receiver<Option<Update>>,
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
    changes: &mut broadcast::Receiver<Option<Update>>,
) -> ApiResult<&'static str> {
    let mut expires_at = actor.expires_at;
    let expires = sleep(Duration::ZERO);
    tokio::pin!(expires);
    let mut presence = None;
    let mut pending: HashSet<KeyNeed> = HashSet::new();
    let mut rotations: HashSet<(Scope, i64)> = HashSet::new();
    let mut readable: HashSet<String> = HashSet::new();
    let mut notices = Vec::new();
    let mut lagged = false;
    let mut refresh = true;
    loop {
        // Bound the drain to the broadcast backlog so a busy stream still makes progress.
        for _ in 0..=app.changes.len() {
            use broadcast::error::TryRecvError;
            match changes.try_recv() {
                Ok(notice) => refresh |= queue(&mut notices, notice),
                Err(TryRecvError::Lagged(_)) => lagged = true,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => return Ok("server_closed"),
            }
        }
        refresh |= lagged;
        // Message and voice notices reuse the last session and access state.
        let next = if refresh {
            refresh = false;
            let mut tx = app.db.begin().await?;
            let at = now()?;
            // The bearer changes on refresh. A new sign-in replaces the session ID.
            let renewed: Option<i64> = query_scalar(
                "SELECT expires_at FROM sessions WHERE person_id = ? AND device_id = ? AND session_id = ?",
            )
            .bind(actor.person.as_slice())
            .bind(&actor.device)
            .bind(&actor.session_id)
            .fetch_optional(&mut *tx)
            .await?;
            expires_at = renewed.unwrap_or(expires_at);
            if expires_at <= at {
                return Ok("token_expired");
            }
            if renewed.is_none() {
                return session_end(&mut tx, actor).await;
            }
            expires
                .as_mut()
                .reset(Instant::now() + Duration::from_secs((expires_at - at) as u64));
            let next_pending: HashSet<_> =
                keys::pending(&mut tx, actor).await?.into_iter().collect();
            let scopes = keys::scopes(&mut tx, actor).await?;
            readable = scopes
                .iter()
                .filter(|state| state.scope.kind == Kind::Channel)
                .map(|state| encode(&state.scope.id))
                .collect();
            let next_rotations: HashSet<_> = scopes
                .iter()
                .filter(|state| state.rotate)
                .map(|state| (state.scope, state.epoch))
                .collect();
            let status = if presence.is_none() {
                Some(presence::stored(&mut tx, &actor.person).await?)
            } else {
                None
            };
            tx.commit().await?;
            if let Some(status) = status {
                presence = Some(presence::open(app, encode(&actor.person), status));
            }
            Some((next_pending, next_rotations))
        } else {
            None
        };

        if lagged {
            send(socket, Update::Event(Event::Resync)).await?;
            pending.clear();
            rotations.clear();
            lagged = false;
        }
        let device = rusttalk_core::auth::encode(&actor.device);
        for update in notices.drain(..) {
            let Update::Event(notice) = update else {
                send(socket, update).await?;
                continue;
            };
            if let Event::KeysNeeded(need) = &notice {
                // A rejected slot can reopen within this batch and still need a wrap.
                if let Some((next_pending, _)) = &next
                    && next_pending.contains(need)
                {
                    pending.remove(need);
                    continue;
                }
                // A completed wrap only wakes its recipient.
                if need.device != device {
                    continue;
                }
            }
            if let Event::LiveFileAccepted { acceptance, .. } = &notice
                && acceptance.recipient != device
            {
                continue;
            }
            let channel = match &notice {
                Event::DmCreated { channel }
                | Event::VoiceChanged { channel }
                | Event::LiveFileAccepted { channel, .. }
                | Event::MessageCreated { channel, .. }
                | Event::MessageDeleted { channel, .. } => Some(channel.clone()),
                Event::KeysNeeded(need) if need.scope.kind == Kind::Channel => {
                    Some(encode(&need.scope.id))
                }
                _ => None,
            };
            if channel.is_none_or(|id| readable.contains(&id)) {
                send(socket, Update::Event(notice)).await?;
            }
        }
        if let Some((next_pending, next_rotations)) = next {
            for need in next_pending.difference(&pending) {
                send(socket, Update::Event(Event::KeysNeeded(need.clone()))).await?;
            }
            for &(scope, _) in next_rotations.difference(&rotations) {
                send(socket, Update::Event(Event::RotateNeeded { scope })).await?;
            }
            pending = next_pending;
            rotations = next_rotations;
        }

        loop {
            tokio::select! {
                _ = &mut expires => {
                    refresh = true;
                    break;
                },
                incoming = socket.recv() => match incoming {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    None | Some(Ok(Message::Close(_))) => return Ok("client_left"),
                    Some(Ok(_)) => return Ok("unexpected_client_message"),
                    Some(Err(_)) => return Ok("socket_io_error"),
                },
                update = changes.recv() => {
                    match update {
                        Ok(notice) => refresh |= queue(&mut notices, notice),
                        Err(broadcast::error::RecvError::Lagged(_)) => lagged = true,
                        Err(broadcast::error::RecvError::Closed) => return Ok("server_closed"),
                    }
                    break;
                },
            }
        }
    }
}

/// Queue a notice and report whether it can change session, access or key state.
fn queue(notices: &mut Vec<Update>, notice: Option<Update>) -> bool {
    let refresh = !matches!(
        notice,
        Some(Update::Presence(_))
            | Some(Update::Event(
                Event::MessageCreated { .. }
                    | Event::MessageDeleted { .. }
                    | Event::VoiceChanged { .. }
                    | Event::LiveFileAccepted { .. }
            ))
    );
    notices.extend(notice);
    refresh
}

async fn send(socket: &mut WebSocket, notice: Update) -> ApiResult<()> {
    let text = serde_json::to_string(&notice).map_err(|_| ApiError::internal("event_encoding"))?;
    timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(text.into())),
    )
    .await
    .map_err(|_| ApiError::internal("event_timeout"))?
    .map_err(|_| ApiError::internal("socket_io_error"))
}

async fn session_end(db: &mut SqliteConnection, actor: &Actor) -> ApiResult<&'static str> {
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
    Ok(match reason.as_str() {
        "banned" => "banned",
        "kicked" => "kicked",
        "revoked" => "revoked",
        _ => "session_replaced",
    })
}
