use std::{
    collections::{BTreeSet, HashMap},
    env,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
};
use ed25519_dalek::VerifyingKey;
use livekit_api::{
    access_token::{AccessToken, TokenVerifier, VideoGrants},
    webhooks::WebhookReceiver,
};
use livekit_protocol::{ParticipantPermission, TrackSource};
use rusttalk_core::{
    auth::{decode, encode, live_file_identity, voice_identity},
    keys::Event,
    messages::LiveFileAcceptance,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{SqliteConnection, query_as, query_scalar};
use zeroize::Zeroizing;

use crate::{ApiError, ApiResult, App, auth::authenticate_read, channels, messages, payload};

pub mod moderation;

const DEV_SECRET: &str = "dev-only-livekit-secret-do-not-deploy";
const DATA_PREFIX: &str = "file-";
const ROSTER_TTL: Duration = Duration::from_secs(1);

/// LiveKit identities active in a room.
type Active = BTreeSet<String>;

pub struct LiveKit {
    api_key: String,
    api_secret: Zeroizing<String>,
    webhook: WebhookReceiver,
    http: reqwest::Client,
    /// Active identities per room and when they were asked for; `None` marks a room
    /// LiveKit reported a change in at that time, so older answers are not used again.
    rosters: Mutex<HashMap<String, (Instant, Option<Active>)>>,
}

impl LiveKit {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        if env::var_os("LIVEKIT_API_KEY").is_none() && env::var_os("LIVEKIT_API_SECRET").is_none() {
            return Ok(None);
        }
        let api_key = env::var("LIVEKIT_API_KEY").context("missing LiveKit API key")?;
        let api_secret =
            Zeroizing::new(env::var("LIVEKIT_API_SECRET").context("missing LiveKit API secret")?);
        ensure!(
            !api_key.is_empty() && !api_secret.is_empty(),
            "empty LiveKit credentials"
        );
        ensure!(
            *api_secret != DEV_SECRET || env::var("RUSTTALK_DEV").as_deref() == Ok("1"),
            "refusing the public development LiveKit secret; set a private LIVEKIT_API_SECRET"
        );
        Ok(Some(Self {
            webhook: WebhookReceiver::new(TokenVerifier::with_api_key(&api_key, &api_secret)),
            api_key,
            api_secret,
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()?,
            rosters: Mutex::default(),
        }))
    }

    async fn room_service(
        &self,
        method: &str,
        room: &str,
        body: serde_json::Value,
    ) -> ApiResult<reqwest::Response> {
        let token = self.sign(
            "rusttalk-api",
            VideoGrants {
                room: room.into(),
                room_admin: method != "ListRooms",
                room_list: method == "ListRooms",
                room_create: method == "DeleteRoom",
                ..Default::default()
            },
            Duration::from_secs(30),
        )?;
        self.http
            .post(format!(
                "http://livekit:7880/twirp/livekit.RoomService/{method}"
            ))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "livekit_unreachable"))
    }

    async fn rooms(&self) -> ApiResult<Vec<String>> {
        #[derive(Deserialize)]
        struct Rooms {
            #[serde(default)]
            rooms: Vec<Room>,
        }
        #[derive(Deserialize)]
        struct Room {
            name: String,
        }
        let unavailable = |_| ApiError::new(StatusCode::BAD_GATEWAY, "livekit_rooms_unavailable");
        let response = self
            .room_service("ListRooms", "", json!({}))
            .await?
            .error_for_status()
            .map_err(unavailable)?;
        let result: Rooms = response.json().await.map_err(unavailable)?;
        Ok(result.rooms.into_iter().map(|room| room.name).collect())
    }

    async fn list(&self, room: &str) -> ApiResult<Vec<Participant>> {
        let unavailable = || ApiError::new(StatusCode::BAD_GATEWAY, "livekit_roster_unavailable");
        let response = self
            .room_service("ListParticipants", room, json!({ "room": room }))
            .await
            .map_err(|_| unavailable())?;
        match response.status() {
            StatusCode::OK => Ok(response
                .json::<Roster>()
                .await
                .map_err(|_| unavailable())?
                .participants),
            StatusCode::NOT_FOUND => Ok(Vec::new()),
            _ => Err(unavailable()),
        }
    }

    async fn active(&self, room: &str) -> ApiResult<BTreeSet<String>> {
        if let Some((at, Some(active))) = self.rosters.lock().expect("roster cache").get(room)
            && at.elapsed() < ROSTER_TTL
        {
            return Ok(active.clone());
        }
        let asked = Instant::now();
        let active: BTreeSet<_> = self
            .list(room)
            .await?
            .into_iter()
            .filter(|participant| participant.state == "ACTIVE")
            .map(|participant| participant.identity)
            .collect();
        self.remember(room, asked, active.clone());
        Ok(active)
    }

    /// Cache a roster LiveKit gave for a request sent at `asked`.
    fn remember(&self, room: &str, asked: Instant, active: BTreeSet<String>) {
        let mut rosters = self.rosters.lock().expect("roster cache");
        // Expired rooms are dropped so closed rooms' participants are not kept in memory.
        rosters.retain(|_, (at, _)| at.elapsed() < ROSTER_TTL);
        // An answer from before a reported change may still list who just left.
        if rosters.get(room).is_none_or(|(at, _)| *at < asked) {
            rosters.insert(room.into(), (asked, Some(active)));
        }
    }

    /// Someone joined or left `room`: the refetch its notice triggers must not get the
    /// cached roster from before, or a person who switched channels stays listed in both.
    fn changed(&self, room: &str) {
        let mut rosters = self.rosters.lock().expect("roster cache");
        rosters.insert(room.into(), (Instant::now(), None));
    }

    async fn moderate(&self, room: &str, method: &str, body: serde_json::Value) {
        let reason = match self.room_service(method, room, body).await {
            Ok(response) if response.status().is_success() => {
                tracing::info!(method, "changed voice room");
                return;
            }
            // The participant left, or the room closed, on its own in the meantime.
            Ok(response) if response.status() == StatusCode::NOT_FOUND => return,
            Ok(_) => "livekit_rejected",
            Err(error) => error.code,
        };
        tracing::warn!(method, reason, "could not change voice room");
    }

    fn sign(&self, identity: &str, grants: VideoGrants, ttl: Duration) -> ApiResult<String> {
        AccessToken::with_api_key(&self.api_key, &self.api_secret)
            .with_identity(identity)
            .with_grants(grants)
            .with_ttl(ttl)
            .to_jwt()
            .map_err(|_| ApiError::internal("livekit_token_failed"))
    }
}

fn configured(app: &App) -> ApiResult<&LiveKit> {
    app.voice
        .as_deref()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "voice_unavailable"))
}

async fn require_voice_read(
    db: &mut SqliteConnection,
    channel: &str,
    person: &[u8],
) -> ApiResult<()> {
    decode::<32>(channel)?;
    let kind = messages::readable_kind(db, channel, person).await?;
    if kind != "voice" {
        return Err(ApiError::bad_request("not_a_voice_channel"));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct Token {
    token: String,
    /// The channel's Opus bitrate, so a joining client publishes at it from the start.
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_kbps: Option<i64>,
}

pub async fn token(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<Json<Token>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    require_voice_read(&mut tx, &channel, &actor.person).await?;
    let sources = publish_sources(&mut tx, &channel, &actor.person).await?;
    let can_publish_data =
        channels::has_flag(&mut tx, &channel, &actor.person, channels::WRITE).await?;
    let identity = voice_identity(app.key.verifying_key().as_bytes(), &actor.device);
    let token = configured(&app)?.sign(
        &identity,
        VideoGrants {
            room: channel.clone(),
            room_join: true,
            can_publish: Some(!sources.is_empty()),
            can_publish_data: Some(can_publish_data),
            can_subscribe: Some(true),
            can_update_own_metadata: Some(true),
            can_publish_sources: sources
                .into_iter()
                .map(|source| source.as_str_name().to_ascii_lowercase())
                .collect(),
            ..Default::default()
        },
        Duration::from_secs(600),
    )?;
    let audio_kbps = query_scalar("SELECT audio_kbps FROM channels WHERE id = ?")
        .bind(&channel)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(Token { token, audio_kbps }))
}

pub async fn data_token(
    State(app): State<App>,
    headers: HeaderMap,
    Path((channel, nonce)): Path<(String, String)>,
) -> ApiResult<Json<Token>> {
    let nonce = decode(&nonce)?;
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    let can_publish_data = require_chat_read(&mut tx, &channel, &actor.person).await?;
    let identity = live_file_identity(app.key.verifying_key().as_bytes(), &actor.device, &nonce);
    let token = configured(&app)?.sign(
        &identity,
        VideoGrants {
            room: format!("{DATA_PREFIX}{channel}"),
            room_join: true,
            can_publish: Some(false),
            can_publish_data: Some(can_publish_data),
            can_subscribe: Some(true),
            ..Default::default()
        },
        Duration::from_secs(60),
    )?;
    tx.commit().await?;
    Ok(Json(Token {
        token,
        audio_kbps: None,
    }))
}

/// Every chat (text, voice or DM) has a data room. Readers may receive; only
/// writers may send. Returns whether this person may publish data.
async fn require_chat_read(
    db: &mut SqliteConnection,
    channel: &str,
    person: &[u8],
) -> ApiResult<bool> {
    decode::<32>(channel)?;
    let kind = messages::readable_kind(db, channel, person).await?;
    messages::can_write(db, channel, person, &kind).await
}

async fn publish_sources(
    db: &mut SqliteConnection,
    channel: &str,
    person: &[u8],
) -> ApiResult<Vec<TrackSource>> {
    let mut sources = Vec::new();
    if channels::has_flag(db, channel, person, channels::SPEAK).await? {
        // A moderator's mute takes the microphone only, as shared app audio is separate.
        if !moderation::muted(db, person).await? {
            sources.push(TrackSource::Microphone);
        }
        sources.push(TrackSource::ScreenShareAudio);
    }
    if channels::has_flag(db, channel, person, channels::VIDEO).await? {
        sources.extend([TrackSource::Camera, TrackSource::ScreenShare]);
    }
    Ok(sources)
}

#[derive(Deserialize)]
struct Participant {
    identity: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    permission: ParticipantPermission,
}

#[derive(Deserialize)]
struct Roster {
    #[serde(default)]
    participants: Vec<Participant>,
}

#[derive(Serialize)]
pub struct Presence {
    people: BTreeSet<String>,
    unknown: usize,
}

/// Map reader devices of a channel to their LiveKit identities.
async fn reader_identities(
    app: &App,
    db: &mut SqliteConnection,
    channel: &str,
) -> ApiResult<HashMap<String, Vec<u8>>> {
    let devices: Vec<(Vec<u8>, Vec<u8>)> = query_as(
        "SELECT d.device_id, d.person_id FROM channel_readers r
         JOIN member_devices d ON d.person_id = r.person_id
         WHERE r.channel_id = ?",
    )
    .bind(channel)
    .fetch_all(db)
    .await?;
    let server = app.key.verifying_key();
    Ok(devices
        .into_iter()
        .map(|(device, person)| (voice_identity(server.as_bytes(), &device), person))
        .collect())
}

pub async fn participants(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<Json<Presence>> {
    let (mut tx, actor) = authenticate_read(&app, &headers).await?;
    require_voice_read(&mut tx, &channel, &actor.person).await?;
    tx.commit().await?;
    let active = configured(&app)?.active(&channel).await?;

    // Recheck access after the network call, which can overlap a kick or permission change.
    let (mut tx, _) = authenticate_read(&app, &headers).await?;
    require_voice_read(&mut tx, &channel, &actor.person).await?;
    let readers = reader_identities(&app, &mut tx, &channel).await?;
    tx.commit().await?;
    let mut presence = Presence {
        people: BTreeSet::new(),
        unknown: 0,
    };
    for identity in active {
        match readers.get(&identity) {
            Some(person) => {
                presence.people.insert(encode(person));
            }
            None => presence.unknown += 1,
        }
    }
    Ok(Json(presence))
}

/// Disconnect devices that lost read access and revoke lost publication grants.
/// Runs after the change commits and never blocks it; LiveKit keeps the grants a device
/// joined with, and refreshes its token with them, until told otherwise.
pub fn enforce(app: &App, room: Option<String>) {
    let Some(livekit) = app.voice.clone() else {
        return;
    };
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(error) = enforce_rooms(&app, &livekit, room).await {
            tracing::warn!(reason = error.code, "could not check voice access");
        }
    });
}

/// End the LiveKit room of a deleted voice channel, which `enforce` no longer visits.
/// Runs after the delete commits and never blocks it.
pub fn close_rooms(app: &App, channel: String, voice: bool) {
    let Some(livekit) = app.voice.clone() else {
        return;
    };
    tokio::spawn(async move {
        let data = format!("{DATA_PREFIX}{channel}");
        livekit
            .moderate(&data, "DeleteRoom", json!({ "room": data }))
            .await;
        if voice {
            livekit
                .moderate(&channel, "DeleteRoom", json!({ "room": channel }))
                .await;
        }
    });
}

fn room_channel(room: &str) -> Option<(&str, bool)> {
    if decode::<32>(room).is_ok() {
        return Some((room, false));
    }
    let channel = room.strip_prefix(DATA_PREFIX)?;
    decode::<32>(channel).ok()?;
    Some((channel, true))
}

async fn enforce_rooms(app: &App, livekit: &LiveKit, room: Option<String>) -> ApiResult<()> {
    let rooms = match room {
        Some(channel) => vec![format!("{DATA_PREFIX}{channel}"), channel],
        None => livekit.rooms().await?,
    };
    for room in rooms {
        let Some((channel, data)) = room_channel(&room) else {
            continue;
        };
        let participants = match livekit.list(&room).await {
            Ok(participants) => participants,
            Err(error) => {
                tracing::warn!(reason = error.code, "could not check voice access");
                continue;
            }
        };
        if participants.is_empty() {
            continue;
        }
        // Decide first so no pooled connection is held across LiveKit calls.
        let (mut remove, mut restrict) = (Vec::new(), Vec::new());
        {
            let mut db = app.db.acquire().await?;
            let readers = reader_identities(app, &mut db, channel).await?;
            let kind: Option<String> = query_scalar("SELECT kind FROM channels WHERE id = ?")
                .bind(channel)
                .fetch_optional(&mut *db)
                .await?;
            for participant in participants {
                let device = if data {
                    participant
                        .identity
                        .split_once('.')
                        .map_or("", |(device, _)| device)
                } else {
                    &participant.identity
                };
                let Some(person) = readers.get(device) else {
                    remove.push(participant.identity);
                    continue;
                };
                if data {
                    // Readers stay to receive; a sender that lost write leaves.
                    if participant.permission.can_publish_data
                        && !messages::can_write(
                            &mut db,
                            channel,
                            person,
                            kind.as_deref().unwrap_or(""),
                        )
                        .await?
                    {
                        remove.push(participant.identity);
                    }
                    continue;
                }
                let mut permission = participant.permission;
                let revoke_data = permission.can_publish_data
                    && !channels::has_flag(&mut db, channel, person, channels::WRITE).await?;
                let mut changed = revoke_data;
                if revoke_data {
                    permission.can_publish_data = false;
                }
                if permission.can_publish {
                    let allowed = publish_sources(&mut db, channel, person).await?;
                    let granted = &mut permission.can_publish_sources;
                    let before = granted.len();
                    if before == 0 {
                        // LiveKit represents unrestricted grants with an empty source list.
                        granted.extend(allowed.iter().map(|source| *source as i32));
                    } else {
                        granted.retain(|source| {
                            allowed.iter().any(|allowed| *allowed as i32 == *source)
                        });
                    }
                    if before == 0 || granted.len() != before {
                        permission.can_publish = !granted.is_empty();
                        changed = true;
                    }
                }
                if changed {
                    restrict.push((participant.identity, permission));
                }
            }
        }
        for identity in remove {
            let body = json!({ "room": room, "identity": identity });
            livekit.moderate(&room, "RemoveParticipant", body).await;
        }
        for (identity, permission) in restrict {
            let body = json!({
                "room": room,
                "identity": identity,
                "permission": permission,
            });
            livekit.moderate(&room, "UpdateParticipant", body).await;
        }
    }
    Ok(())
}

/// Relay a device-signed acceptance to the offerer through the existing live events socket.
pub async fn live_file(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    body: Result<Json<LiveFileAcceptance>, JsonRejection>,
) -> ApiResult<StatusCode> {
    relay_acceptance(&app, &headers, channel, payload(body)?, false).await
}

pub async fn data_accept(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    body: Result<Json<LiveFileAcceptance>, JsonRejection>,
) -> ApiResult<StatusCode> {
    relay_acceptance(&app, &headers, channel, payload(body)?, true).await
}

async fn relay_acceptance(
    app: &App,
    headers: &HeaderMap,
    channel: String,
    acceptance: LiveFileAcceptance,
    data: bool,
) -> ApiResult<StatusCode> {
    let (mut tx, actor) = authenticate_read(app, headers).await?;
    if data {
        require_chat_read(&mut tx, &channel, &actor.person).await?;
    } else {
        require_voice_read(&mut tx, &channel, &actor.person).await?;
    }
    let device = actor
        .device
        .as_slice()
        .try_into()
        .ok()
        .and_then(|id| VerifyingKey::from_bytes(id).ok())
        .ok_or_else(|| ApiError::internal("invalid_stored_device"))?;
    acceptance
        .verify(
            app.key.verifying_key().as_bytes(),
            &decode(&channel)?,
            &device,
        )
        .map_err(|_| ApiError::bad_request("invalid_file_acceptance"))?;
    let recipient = decode::<32>(&acceptance.recipient)?;
    if recipient.as_slice() == actor.device {
        return Err(ApiError::forbidden("live_file_peer_not_in_room"));
    }
    if data && !acceptance.cancel {
        let person: Vec<u8> =
            query_scalar("SELECT person_id FROM member_devices WHERE device_id = ?")
                .bind(recipient.as_slice())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(ApiError::forbidden("channel_read_denied"))?;
        messages::require_writer(&mut tx, &channel, &person).await?;
    }
    tx.commit().await?;
    if !acceptance.cancel {
        let server = app.key.verifying_key();
        let (participants, required) = if data {
            let nonce = decode(&acceptance.nonce)?;
            let receiver = live_file_identity(server.as_bytes(), &actor.device, &nonce);
            let room = format!("{DATA_PREFIX}{channel}");
            (configured(app)?.list(&room).await?, vec![receiver])
        } else {
            // Receive-only participants need not appear in the SDK’s remote participant map.
            let receiver = voice_identity(server.as_bytes(), &actor.device);
            let offerer = voice_identity(server.as_bytes(), &recipient);
            (
                configured(app)?.list(&channel).await?,
                vec![receiver, offerer],
            )
        };
        if required.iter().any(|identity| {
            !participants
                .iter()
                .any(|p| &p.identity == identity && p.state == "ACTIVE")
        }) {
            return Err(ApiError::forbidden("live_file_peer_not_in_room"));
        }
    }
    app.notify(Event::LiveFileAccepted {
        channel,
        device: encode(&actor.device),
        acceptance,
        data,
    });
    Ok(StatusCode::NO_CONTENT)
}

pub async fn webhook(
    State(app): State<App>,
    headers: HeaderMap,
    body: String,
) -> ApiResult<StatusCode> {
    let invalid = || ApiError::unauthorized("invalid_livekit_webhook");
    let token = headers
        .get(AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .ok_or_else(invalid)?;
    let livekit = configured(&app)?;
    let event = livekit
        .webhook
        .receive(&body, token)
        .map_err(|_| invalid())?;
    if matches!(
        event.event.as_str(),
        "participant_joined" | "participant_left" | "room_finished"
    ) && let Some(room) = event.room
        && let Some((channel, data)) = room_channel(&room.name)
    {
        if event.event == "participant_joined" {
            // A LiveKit token outlives the session that requested it.
            enforce(&app, Some(channel.to_owned()));
        }
        if !data {
            livekit.changed(&room.name);
            app.notify(Event::VoiceChanged { channel: room.name });
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn only_voice_tokens_allow_updating_own_metadata() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = crate::store::open(dir.path()).await?;
        let api_key = "test-key";
        let api_secret = "test-secret-with-at-least-32-characters";
        let verifier = TokenVerifier::with_api_key(api_key, api_secret);
        let app = App {
            emoji: crate::emoji::Emoji::open(dir.path(), &db).await?.into(),
            files: crate::files::Files::open(dir.path(), &db).await?.into(),
            db,
            key: Arc::new(ed25519_dalek::SigningKey::from_bytes(&[7; 32])),
            changes: tokio::sync::broadcast::channel(128).0,
            presence: Default::default(),
            voice: Some(Arc::new(LiveKit {
                api_key: api_key.into(),
                api_secret: Zeroizing::new(api_secret.into()),
                webhook: WebhookReceiver::new(TokenVerifier::with_api_key(api_key, api_secret)),
                http: reqwest::Client::new(),
                rosters: Mutex::default(),
            })),
            session_ttl: 3600,
            listing_changed: tokio::sync::watch::channel(()).0,
        };
        let channel = encode(&[20; 32]);
        let person = [1_u8; 32];
        sqlx::query("INSERT INTO members (person_id, joined_at) VALUES (?, 0)")
            .bind(person.as_slice())
            .execute(&app.db)
            .await?;
        sqlx::query("UPDATE server_settings SET owner_person_id = ?")
            .bind(person.as_slice())
            .execute(&app.db)
            .await?;
        sqlx::query("INSERT INTO sessions (token_hash, person_id, device_id, ip, expires_at, session_id) VALUES (?, ?, ?, '192.0.2.1', ?, x'01')")
            .bind(crate::store::hash(&[1; 32]).as_slice()).bind(person.as_slice()).bind([2; 32].as_slice())
            .bind(i64::MAX).execute(&app.db).await?;
        sqlx::query("INSERT INTO categories (id, position, name_ct) VALUES ('cat', 0, x'00')")
            .execute(&app.db)
            .await?;
        sqlx::query("INSERT INTO channels (id, kind, category_id, position, name_ct) VALUES (?, 'voice', 'cat', 0, x'00')")
            .bind(&channel).execute(&app.db).await?;
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {}", encode(&[1; 32])).parse()?,
        );
        headers.insert("x-forwarded-for", "192.0.2.1".parse()?);

        let Json(voice) = token(State(app.clone()), headers.clone(), Path(channel.clone()))
            .await
            .unwrap();
        let claims = verifier.verify(&voice.token)?;
        assert!(claims.video.can_update_own_metadata());
        let Json(data) = data_token(State(app), headers, Path((channel, encode(&[3; 16]))))
            .await
            .unwrap();
        let claims = verifier.verify(&data.token)?;
        assert!(!claims.video.can_update_own_metadata());
        Ok(())
    }

    #[test]
    fn voice_room_ids_can_start_with_the_data_prefix() {
        let channel = format!("{DATA_PREFIX}{}", "A".repeat(38));
        let data_room = format!("{DATA_PREFIX}{channel}");
        assert_eq!(room_channel(&channel), Some((channel.as_str(), false)));
        assert_eq!(room_channel(&data_room), Some((channel.as_str(), true)));
        assert_eq!(room_channel(DATA_PREFIX), None);
    }

    #[test]
    fn a_reported_join_or_leave_drops_older_rosters() {
        let secret = "test-secret-with-at-least-32-characters";
        let livekit = LiveKit {
            api_key: "test-key".into(),
            api_secret: Zeroizing::new(secret.into()),
            webhook: WebhookReceiver::new(TokenVerifier::with_api_key("test-key", secret)),
            http: reqwest::Client::new(),
            rosters: Mutex::default(),
        };
        let cached = |livekit: &LiveKit| {
            let rosters = livekit.rosters.lock().unwrap();
            rosters.get("room").and_then(|(_, active)| active.clone())
        };
        let phil = BTreeSet::from(["phil".to_owned()]);
        let before = Instant::now();
        livekit.remember("room", before, phil.clone());
        assert_eq!(cached(&livekit), Some(phil.clone()));
        // Phil switched away: the notice's refetch must not see him from the cache,
        // even when a request sent before the leave answers late.
        livekit.changed("room");
        assert_eq!(cached(&livekit), None);
        livekit.remember("room", before, phil);
        assert_eq!(cached(&livekit), None);
        livekit.remember(
            "room",
            Instant::now() + Duration::from_millis(1),
            BTreeSet::new(),
        );
        assert_eq!(cached(&livekit), Some(BTreeSet::new()));
    }
}
