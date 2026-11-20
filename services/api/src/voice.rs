use std::{collections::BTreeSet, env, time::Duration};

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
use rusttalk_core::{
    auth::{decode, encode, voice_identity},
    keys::Event,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{SqliteConnection, query_as, query_scalar};
use zeroize::Zeroizing;

use crate::{ApiError, ApiResult, App, auth::authenticate, channels};

pub struct LiveKit {
    api_key: String,
    api_secret: Zeroizing<String>,
    webhook: WebhookReceiver,
    http: reqwest::Client,
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
        Ok(Some(Self {
            webhook: WebhookReceiver::new(TokenVerifier::with_api_key(&api_key, &api_secret)),
            api_key,
            api_secret,
            http: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(3))
                .build()?,
        }))
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
    let speak = channels::has_flag(&mut tx, &channel, &actor.person, channels::SPEAK).await?;
    let identity = voice_identity(app.key.verifying_key().as_bytes(), &actor.device);
    let token = configured(&app)?.sign(
        &identity,
        VideoGrants {
            room: channel,
            room_join: true,
            can_publish: Some(speak),
            can_publish_data: Some(true),
            can_subscribe: Some(true),
            can_publish_sources: if speak {
                vec!["microphone".into()]
            } else {
                Vec::new()
            },
            ..Default::default()
        },
        Duration::from_secs(600),
    )?;
    tx.commit().await?;
    Ok(Json(Token { token }))
}

#[derive(Deserialize)]
struct Participant {
    identity: String,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
struct Roster {
    #[serde(default)]
    participants: Vec<Participant>,
}

pub async fn participants(
    State(app): State<App>,
    headers: HeaderMap,
    Path(channel): Path<String>,
) -> ApiResult<Json<BTreeSet<String>>> {
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    require_voice_read(&mut tx, &channel, &actor.person).await?;
    tx.commit().await?;

    let livekit = configured(&app)?;
    let token = livekit.sign(
        "rusttalk-api",
        VideoGrants {
            room: channel.clone(),
            room_admin: true,
            ..Default::default()
        },
        Duration::from_secs(30),
    )?;
    let unavailable = || ApiError::new(StatusCode::BAD_GATEWAY, "livekit_roster_unavailable");
    let response = livekit
        .http
        .post("http://livekit:7880/twirp/livekit.RoomService/ListParticipants")
        .bearer_auth(token)
        .json(&json!({ "room": channel }))
        .send()
        .await
        .map_err(|_| unavailable())?;
    let roster = match response.status() {
        StatusCode::OK => {
            response
                .json::<Roster>()
                .await
                .map_err(|_| unavailable())?
                .participants
        }
        StatusCode::NOT_FOUND => Vec::new(),
        _ => return Err(unavailable()),
    };
    let active: BTreeSet<_> = roster
        .into_iter()
        .filter(|participant| participant.state == "ACTIVE")
        .map(|participant| participant.identity)
        .collect();

    // Recheck access after the network call, which can overlap a kick or permission change.
    let (mut tx, _) = authenticate(&app, &headers).await?;
    require_voice_read(&mut tx, &channel, &actor.person).await?;
    let devices: Vec<(Vec<u8>, Vec<u8>)> = query_as(
        "SELECT d.device_id, d.person_id FROM channel_readers r
         JOIN member_devices d ON d.person_id = r.person_id
         WHERE r.channel_id = ?",
    )
    .bind(&channel)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let server = app.key.verifying_key();
    Ok(Json(
        devices
            .into_iter()
            .filter(|(device, _)| active.contains(&voice_identity(server.as_bytes(), device)))
            .map(|(_, person)| encode(&person))
            .collect(),
    ))
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
        app.notify(Some(Event::VoiceChanged { channel: room.name }));
    }
    Ok(StatusCode::NO_CONTENT)
}
