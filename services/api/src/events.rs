use std::{collections::HashSet, sync::Arc, time::Duration};

use axum::{
    extract::{
        State, WebSocketUpgrade,
        ws::{CloseFrame, Message, Utf8Bytes, WebSocket, close_code},
    },
    http::{HeaderMap, Uri},
    response::Response,
};
use rusttalk_core::{
    auth::encode,
    keys::{Event, KeyNeed, Kind, Scope},
    messages::MessageRecord,
    presence::Update,
};
use sqlx::{SqliteConnection, query_scalar};
use tokio::{
    sync::broadcast,
    time::{Instant, sleep, timeout},
};
use tracing::Instrument;

use crate::{
    ApiError, ApiResult, App, Change,
    auth::{Actor, authenticate_read},
    keys, presence,
    store::now,
};

/// A client update and its JSON, encoded once when broadcast and shared by every socket.
pub struct Notice {
    pub update: Update,
    /// `None` only if encoding failed; each socket then closes as it did before.
    text: Option<Utf8Bytes>,
}

impl Change {
    pub fn update(update: Update) -> Self {
        let text = match serde_json::to_string(&update) {
            Ok(text) => Some(text.into()),
            Err(error) => {
                tracing::error!(%error, "event encoding failed");
                None
            }
        };
        Change::Update(Arc::new(Notice { update, text }))
    }
}

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
    let (tx, actor) = authenticate_read(&app, &headers).await?;
    tx.commit().await?;
    Ok(ws
        .max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |socket| serve(app, socket, actor, changes).instrument(span)))
}

/// Largest event that carries an inline record. Clients up to v0.3.0 drop the socket on
/// frames over 4,096 bytes; the margin covers the frame header.
const MAX_INLINE_EVENT_BYTES: usize = 3_800;

/// A new message's event, carrying its record when it is small enough for every client.
pub fn message_created(channel: String, id: String, record: &MessageRecord) -> Event {
    let mut event = Event::MessageCreated {
        channel,
        id,
        record: Some(Box::new(record.clone())),
    };
    if !serde_json::to_vec(&event).is_ok_and(|json| json.len() <= MAX_INLINE_EVENT_BYTES)
        && let Event::MessageCreated { record, .. } = &mut event
    {
        *record = None;
    }
    event
}

async fn serve(
    app: App,
    mut socket: WebSocket,
    actor: Actor,
    mut changes: broadcast::Receiver<Change>,
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
    changes: &mut broadcast::Receiver<Change>,
) -> ApiResult<&'static str> {
    let mut expires_at = actor.expires_at;
    // The first pass reads the session and arms this.
    let expires = sleep(Duration::ZERO);
    tokio::pin!(expires);
    let mut presence = None;
    let mut pending: HashSet<KeyNeed> = HashSet::new();
    let mut rotations: HashSet<(Scope, i64)> = HashSet::new();
    let mut readable: HashSet<String> = HashSet::new();
    let mut notices = Vec::new();
    let mut lagged = false;
    let mut recheck = Recheck::Access;
    let device = rusttalk_core::auth::encode(&actor.device);
    loop {
        // Bound the drain to the broadcast backlog so a busy stream still makes progress.
        for _ in 0..=app.changes.len() {
            use broadcast::error::TryRecvError;
            match changes.try_recv() {
                Ok(change) => recheck = recheck.max(queue(&mut notices, change, actor, &device)),
                Err(TryRecvError::Lagged(_)) => lagged = true,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => return Ok("server_closed"),
            }
        }
        if lagged {
            recheck = Recheck::Access;
        }
        // Message and voice notices reuse the last session and access state. Sign-ins only
        // recheck this socket's session; key and membership changes also recheck access.
        let current = std::mem::replace(&mut recheck, Recheck::None);
        let mut tx = None;
        if current != Recheck::None {
            let mut db = app.db.begin().await?;
            expires_at = match session(&mut db, actor, expires_at).await? {
                Ok(at) => at,
                Err(reason) => return Ok(reason),
            };
            expires
                .as_mut()
                .reset(Instant::now() + Duration::from_secs((expires_at - now()?).max(0) as u64));
            // Announce the member once the session is confirmed, in its own short
            // transaction, so others see them online before the heavier key queries run.
            if presence.is_none() {
                let status = presence::stored(&mut db, &actor.person).await?;
                db.commit().await?;
                presence = Some(presence::open(app, encode(&actor.person), status));
                db = app.db.begin().await?;
            }
            tx = Some(db);
        }
        let next = if let Some(mut tx) = tx
            && current == Recheck::Access
        {
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
            tx.commit().await?;
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
        for update in notices.drain(..) {
            let Update::Event(notice) = &update.update else {
                forward(socket, &update).await?;
                continue;
            };
            if let Event::KeysNeeded(need) = &notice {
                // This slot changed without a recheck here, so forget it: if it reopens,
                // a later recheck announces it again.
                pending.remove(need);
                // A rejected slot can reopen within this batch and still need a wrap.
                if let Some((next_pending, _)) = &next
                    && next_pending.contains(need)
                {
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
            if let Event::VoiceMoved { devices, .. } = &notice
                && !devices.contains(&device)
            {
                continue;
            }
            let channel = match &notice {
                Event::DmCreated { channel }
                | Event::VoiceChanged { channel, .. }
                | Event::LiveFileAccepted { channel, .. }
                | Event::MessageCreated { channel, .. }
                | Event::MessageDeleted { channel, .. } => Some(channel.clone()),
                Event::KeysNeeded(need) if need.scope.kind == Kind::Channel => {
                    Some(encode(&need.scope.id))
                }
                _ => None,
            };
            if channel.is_none_or(|id| readable.contains(&id)) {
                forward(socket, &update).await?;
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
                // A refresh keeps the session and moves its expiry, so reread it.
                _ = &mut expires => {
                    recheck = Recheck::Session;
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
                        Ok(change) => recheck = recheck.max(queue(&mut notices, change, actor, &device)),
                        Err(broadcast::error::RecvError::Lagged(_)) => lagged = true,
                        Err(broadcast::error::RecvError::Closed) => return Ok("server_closed"),
                    }
                    break;
                },
            }
        }
    }
}

/// What a socket must recheck before forwarding queued notices.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Recheck {
    None,
    Session,
    Access,
}

/// Queue a client notice and report what it can change: nothing, sessions, or access and keys.
/// `actor` and `device` are this socket's signed-in person and encoded device.
fn queue(notices: &mut Vec<Arc<Notice>>, change: Change, actor: &Actor, device: &str) -> Recheck {
    match change {
        Change::Session(person) if person == actor.person => Recheck::Session,
        Change::Session(_) => Recheck::None,
        Change::Keys => Recheck::Access,
        Change::Access(people) if people.contains(&actor.person) => Recheck::Access,
        Change::Access(_) => Recheck::None,
        Change::Update(update) => {
            let recheck = match &update.update {
                Update::Presence(_)
                | Update::Event(
                    Event::MessageCreated { .. }
                    | Event::MessageDeleted { .. }
                    | Event::VoiceChanged { .. }
                    | Event::VoiceMoved { .. }
                    | Event::LiveFileAccepted { .. }
                    // Its two people recheck through the `Access` change sent just before it.
                    | Event::DmCreated { .. },
                ) => Recheck::None,
                // A wrap changes only its recipient's keys; a rejection also signals `Keys`.
                Update::Event(Event::KeysNeeded(need)) if need.device != device => Recheck::None,
                // Profile and listing edits change no keys or access.
                Update::Event(event) if event.keeps_access() => Recheck::None,
                Update::Event(_) => Recheck::Access,
            };
            notices.push(update);
            recheck
        }
    }
}

/// Send a notice built for this socket alone.
async fn send(socket: &mut WebSocket, notice: Update) -> ApiResult<()> {
    let text = serde_json::to_string(&notice).map_err(|_| ApiError::internal("event_encoding"))?;
    send_text(socket, text.into()).await
}

/// Send a broadcast notice's shared JSON, the same bytes `send` would encode.
async fn forward(socket: &mut WebSocket, notice: &Notice) -> ApiResult<()> {
    let text = (notice.text.clone()).ok_or_else(|| ApiError::internal("event_encoding"))?;
    send_text(socket, text).await
}

async fn send_text(socket: &mut WebSocket, text: Utf8Bytes) -> ApiResult<()> {
    timeout(Duration::from_secs(5), socket.send(Message::Text(text)))
        .await
        .map_err(|_| ApiError::internal("event_timeout"))?
        .map_err(|_| ApiError::internal("socket_io_error"))
}

/// The session's current expiry, or why this socket must close. The bearer changes on
/// refresh; a new sign-in replaces the session ID. `known` is the last expiry seen, so a
/// session the sweeper already removed still reports `token_expired`.
async fn session(
    db: &mut SqliteConnection,
    actor: &Actor,
    known: i64,
) -> ApiResult<Result<i64, &'static str>> {
    let renewed: Option<i64> = query_scalar(
        "SELECT expires_at FROM sessions WHERE person_id = ? AND device_id = ? AND session_id = ?",
    )
    .bind(actor.person.as_slice())
    .bind(&actor.device)
    .bind(&actor.session_id)
    .fetch_optional(&mut *db)
    .await?;
    let expires_at = renewed.unwrap_or(known);
    if expires_at <= now()? {
        return Ok(Err("token_expired"));
    }
    if renewed.is_some() {
        return Ok(Ok(expires_at));
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
    Ok(Err(match reason.as_str() {
        "banned" => "banned",
        "kicked" => "kicked",
        "revoked" => "revoked",
        _ => "session_replaced",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusttalk_core::keys::{MemberProfile, Part};

    fn test_actor() -> Actor {
        Actor {
            person: [1; 32],
            device: vec![2; 32],
            ip: String::new(),
            at: 0,
            session_id: Vec::new(),
            expires_at: 0,
        }
    }

    #[test]
    fn shared_text_matches_per_socket_encoding() {
        use rusttalk_core::{
            messages::LiveFileAcceptance,
            presence::{Presence, PresenceEvent},
        };
        let scope = Scope {
            kind: Kind::Channel,
            id: [7; 32],
        };
        let events = [
            Event::KeysNeeded(KeyNeed {
                scope,
                epoch: 3,
                device: "d".into(),
            }),
            Event::RotateNeeded { scope },
            Event::MemberChanged {
                person: "p".into(),
                device: Some("d".into()),
                profile: None,
            },
            Event::ChannelChanged { part: None },
            Event::VoiceChanged {
                channel: "c".into(),
                people: None,
                unknown: None,
            },
            Event::VoiceMoved {
                devices: vec!["d".into()],
                from: "a".into(),
                to: "b".into(),
            },
            Event::DmCreated {
                channel: "c".into(),
            },
            Event::MessageCreated {
                channel: "c".into(),
                id: "m".into(),
                record: None,
            },
            Event::MessageDeleted {
                channel: "c".into(),
                id: "m".into(),
            },
            Event::LiveFileAccepted {
                channel: "c".into(),
                device: "d".into(),
                acceptance: LiveFileAcceptance {
                    recipient: "r".into(),
                    transfer: "t".into(),
                    nonce: "n".into(),
                    cancel: false,
                    signature: "s".into(),
                },
                data: true,
            },
            Event::Resync,
        ];
        // Fails to build when an event kind is added, so the list above stays complete.
        for event in &events {
            match event {
                Event::KeysNeeded(_)
                | Event::RotateNeeded { .. }
                | Event::MemberChanged { .. }
                | Event::ChannelChanged { .. }
                | Event::VoiceChanged { .. }
                | Event::VoiceMoved { .. }
                | Event::DmCreated { .. }
                | Event::MessageCreated { .. }
                | Event::MessageDeleted { .. }
                | Event::LiveFileAccepted { .. }
                | Event::Resync => {}
            }
        }
        let presence = Update::Presence(PresenceEvent::PresenceChanged {
            person_id: "p".into(),
            status: Presence::Away,
        });
        let updates = events.into_iter().map(Update::Event).chain([presence]);
        for update in updates {
            // What `send` encoded for every socket before.
            let per_socket = serde_json::to_string(&update).unwrap();
            let Change::Update(notice) = Change::update(update) else {
                unreachable!()
            };
            assert_eq!(notice.text.as_deref(), Some(per_socket.as_str()));
        }
    }

    #[test]
    fn sign_ins_recheck_only_sessions() {
        let mut notices = Vec::new();
        let actor = test_actor();
        let queue = |notices: &mut Vec<Arc<Notice>>, change, device: &str| {
            queue(notices, change, &actor, device)
        };
        assert!(queue(&mut notices, Change::Session([1; 32]), "d") == Recheck::Session);
        // Another person's sign-in does not wake this socket.
        assert!(queue(&mut notices, Change::Session([3; 32]), "d") == Recheck::None);
        assert!(queue(&mut notices, Change::Keys, "d") == Recheck::Access);
        let message = Event::MessageCreated {
            channel: "c".into(),
            id: "m".into(),
            record: None,
        };
        assert!(queue(&mut notices, Change::update(Update::Event(message)), "d") == Recheck::None);
        let member = Event::MemberChanged {
            person: "p".into(),
            device: None,
            profile: None,
        };
        assert!(queue(&mut notices, Change::update(Update::Event(member)), "d") == Recheck::Access);
        let channels = |part| Change::update(Update::Event(Event::ChannelChanged { part }));
        assert!(queue(&mut notices, channels(None), "d") == Recheck::Access);
        assert!(queue(&mut notices, channels(Some(Part::Emoji)), "d") == Recheck::None);
        let profile = Event::MemberChanged {
            person: "p".into(),
            device: None,
            profile: Some(Box::new(MemberProfile {
                display_name_ct: Some("ct".into()),
                avatar_hash: None,
                bio_ct: None,
                hide_offline: false,
                avatar_initialized: true,
                voice_muted: false,
            })),
        };
        assert!(queue(&mut notices, Change::update(Update::Event(profile)), "d") == Recheck::None);
        // Only client updates are forwarded.
        assert_eq!(notices.len(), 5);
    }

    #[test]
    fn dm_changes_recheck_only_its_two_people() {
        let mut notices = Vec::new();
        let socket = |person| Actor {
            person,
            ..test_actor()
        };
        let dm = || Change::Access([[1; 32], [5; 32]]);
        // Either person, on any of their devices.
        for (person, device) in [([1; 32], "d"), ([1; 32], "e"), ([5; 32], "d")] {
            assert!(queue(&mut notices, dm(), &socket(person), device) == Recheck::Access);
        }
        assert!(queue(&mut notices, dm(), &socket([3; 32]), "d") == Recheck::None);
        // A new DM's notice wakes nobody; its two people recheck through the change above.
        let created = || {
            Change::update(Update::Event(Event::DmCreated {
                channel: "c".into(),
            }))
        };
        for person in [[1; 32], [3; 32]] {
            assert!(queue(&mut notices, created(), &socket(person), "d") == Recheck::None);
        }
        // A channel's epoch still rechecks everyone.
        for person in [[1; 32], [3; 32]] {
            assert!(queue(&mut notices, Change::Keys, &socket(person), "d") == Recheck::Access);
        }
        // The notice is still queued, so sockets that can read the DM get it.
        assert_eq!(notices.len(), 2);
    }

    #[test]
    fn key_wraps_recheck_only_their_recipient() {
        let mut notices = Vec::new();
        let need = |device: &str| {
            Change::update(Update::Event(Event::KeysNeeded(KeyNeed {
                scope: Scope {
                    kind: Kind::Channel,
                    id: [0; 32],
                },
                epoch: 1,
                device: device.into(),
            })))
        };
        let actor = test_actor();
        assert!(queue(&mut notices, need("other"), &actor, "d") == Recheck::None);
        assert!(queue(&mut notices, need("d"), &actor, "d") == Recheck::Access);
        assert_eq!(notices.len(), 2);
    }

    #[test]
    fn only_records_that_fit_old_clients_go_inline() {
        let record = |envelope: usize| MessageRecord {
            backfilled: false,
            id: [1; 32],
            epoch: 1,
            person: [2; 32],
            sender_device: [3; 32],
            at: 0,
            size: envelope,
            envelope: "e".repeat(envelope),
        };
        let inline = |envelope| match message_created("c".into(), "m".into(), &record(envelope)) {
            Event::MessageCreated { record, .. } => record.map(|r| r.envelope.len()),
            _ => unreachable!(),
        };
        assert_eq!(inline(1_000), Some(1_000));
        // A record that would not fit is left out; readers fetch it as before.
        let big = message_created("c".into(), "m".into(), &record(4_000));
        assert!(matches!(big, Event::MessageCreated { record: None, .. }));
        let json = serde_json::to_string(&Update::Event(big)).unwrap();
        assert_eq!(json, r#"{"type":"message_created","channel":"c","id":"m"}"#);
    }

    /// Others hear a socket is online before its key queries finish, and offline again
    /// when those queries fail and the socket closes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn presence_opens_before_key_queries() -> anyhow::Result<()> {
        use axum::{Router, routing::get};
        use rusttalk_core::presence::{Presence, PresenceEvent};
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::{TcpListener, TcpStream},
        };

        use crate::testing;

        let dir = tempfile::tempdir()?;
        let app = testing::app(dir.path(), 1).await?;
        let mut changes = app.changes.subscribe();
        // Makes `keys::pending` fail after the session check has passed.
        sqlx::query("DROP TABLE key_epochs")
            .execute(&app.db)
            .await?;
        let router = Router::new()
            .route("/v1/events", get(upgrade))
            .with_state(app.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move { axum::serve(listener, router).await });

        let mut client = TcpStream::connect(address).await?;
        let request = format!(
            "GET /v1/events HTTP/1.1\r\nHost: test\r\nConnection: Upgrade\r\n\
             Upgrade: websocket\r\nSec-WebSocket-Version: 13\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Authorization: Bearer {}\r\nX-Forwarded-For: 192.0.2.1\r\n\r\n",
            encode(&[1; 32]),
        );
        client.write_all(request.as_bytes()).await?;
        let mut head = [0; 12];
        client.read_exact(&mut head).await?;
        assert_eq!(&head[9..12], b"101");

        let person = encode(&testing::person(1));
        let mut seen = Vec::new();
        timeout(Duration::from_secs(30), async {
            while seen.last() != Some(&Presence::Offline) {
                if let Change::Update(notice) = changes.recv().await?
                    && let Update::Presence(PresenceEvent::PresenceChanged { person_id, status }) =
                        &notice.update
                    && *person_id == person
                {
                    seen.push(*status);
                }
            }
            anyhow::Ok(())
        })
        .await??;
        assert_eq!(seen, [Presence::Online, Presence::Offline]);
        assert!(app.presence.snapshot().is_empty());
        Ok(())
    }
}
