use std::{
    collections::{BTreeSet, HashMap},
    env,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{Context, ensure};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
};
use livekit_api::{
    access_token::{AccessToken, TokenVerifier, VideoGrants},
    webhooks::WebhookReceiver,
};
use livekit_protocol::{ParticipantPermission, TrackSource};
use rusttalk_core::{
    auth::{decode, encode, voice_identity},
    keys::Event,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{SqliteConnection, query_as, query_scalar};
use zeroize::Zeroizing;

use crate::{ApiError, ApiResult, App, auth::authenticate, channels};

const DEV_SECRET: &str = "dev-only-livekit-secret-do-not-deploy";
const ROSTER_TTL: Duration = Duration::from_secs(1);

pub struct LiveKit {
    api_key: String,
    api_secret: Zeroizing<String>,
    webhook: WebhookReceiver,
    http: reqwest::Client,
    rosters: Mutex<HashMap<String, (Instant, BTreeSet<String>)>>,
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
                room_admin: true,
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
        if let Some((at, active)) = self.rosters.lock().expect("roster cache").get(room)
            && at.elapsed() < ROSTER_TTL
        {
            return Ok(active.clone());
        }
        let active: BTreeSet<_> = self
            .list(room)
            .await?
            .into_iter()
            .filter(|participant| participant.state == "ACTIVE")
            .map(|participant| participant.identity)
            .collect();
        self.rosters
            .lock()
            .expect("roster cache")
            .insert(room.into(), (Instant::now(), active.clone()));
        Ok(active)
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
    let kind: String = query_scalar(
        "SELECT c.kind FROM channels c JOIN channel_readers r ON r.channel_id = c.id
         WHERE c.id = ? AND r.person_id = ?",
    )
    .bind(channel)
    .bind(person)
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::forbidden("channel_read_denied"))?;
    if kind != "voice" {
        return Err(ApiError::bad_request("not_a_voice_channel"));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct Token {
    token: String,
}

pub async fn token(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<Json<Token>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    require_voice_read(&mut tx, &channel, &actor.person).await?;
    let sources = publish_sources(&mut tx, &channel, &actor.person).await?;
    let identity = voice_identity(app.key.verifying_key().as_bytes(), &actor.device);
    let token = configured(&app)?.sign(
        &identity,
        VideoGrants {
            room: channel,
            room_join: true,
            can_publish: Some(!sources.is_empty()),
            // Data channel payloads bypass frame encryption. Live transfer (W12) revisits this.
            can_publish_data: Some(false),
            can_subscribe: Some(true),
            can_publish_sources: sources
                .into_iter()
                .map(|source| source.as_str_name().to_ascii_lowercase())
                .collect(),
            ..Default::default()
        },
        Duration::from_secs(600),
    )?;
    tx.commit().await?;
    Ok(Json(Token { token }))
}

async fn publish_sources(
    db: &mut SqliteConnection,
    channel: &str,
    person: &[u8],
) -> ApiResult<Vec<TrackSource>> {
    let mut sources = Vec::new();
    if channels::has_flag(db, channel, person, channels::SPEAK).await? {
        sources.extend([TrackSource::Microphone, TrackSource::ScreenShareAudio]);
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
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    require_voice_read(&mut tx, &channel, &actor.person).await?;
    tx.commit().await?;
    let active = configured(&app)?.active(&channel).await?;

    // Recheck access after the network call, which can overlap a kick or permission change.
    let (mut tx, _) = authenticate(&app, &headers).await?;
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

/// Disconnect devices that lost read access and revoke publishing from those that lost `speak`.
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
pub fn close_room(app: &App, room: String) {
    let Some(livekit) = app.voice.clone() else {
        return;
    };
    tokio::spawn(async move {
        let body = json!({ "room": room });
        livekit.moderate(&room, "DeleteRoom", body).await;
    });
}

async fn enforce_rooms(app: &App, livekit: &LiveKit, room: Option<String>) -> ApiResult<()> {
    let rooms = match room {
        Some(room) => vec![room],
        None => {
            query_scalar("SELECT id FROM channels WHERE kind = 'voice'")
                .fetch_all(&app.db)
                .await?
        }
    };
    for room in rooms {
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
            let readers = reader_identities(app, &mut db, &room).await?;
            for participant in participants {
                let Some(person) = readers.get(&participant.identity) else {
                    remove.push(participant.identity);
                    continue;
                };
                let mut permission = participant.permission;
                if !permission.can_publish {
                    continue;
                }
                let allowed = publish_sources(&mut db, &room, person).await?;
                let granted = &mut permission.can_publish_sources;
                let before = granted.len();
                if before == 0 {
                    // LiveKit represents unrestricted grants with an empty source list.
                    granted.extend(allowed.iter().map(|source| *source as i32));
                } else {
                    granted
                        .retain(|source| allowed.iter().any(|allowed| *allowed as i32 == *source));
                }
                if before == 0 || granted.len() != before {
                    permission.can_publish = !granted.is_empty();
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
    let event = configured(&app)?
        .webhook
        .receive(&body, token)
        .map_err(|_| invalid())?;
    if matches!(
        event.event.as_str(),
        "participant_joined" | "participant_left" | "room_finished"
    ) && let Some(room) = event.room
        && decode::<32>(&room.name).is_ok()
    {
        if event.event == "participant_joined" {
            // A LiveKit token outlives the session that requested it.
            enforce(&app, Some(room.name.clone()));
        }
        app.notify(Some(Event::VoiceChanged { channel: room.name }));
    }
    Ok(StatusCode::NO_CONTENT)
}
