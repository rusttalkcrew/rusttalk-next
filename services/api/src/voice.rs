use std::{
    collections::{BTreeSet, HashMap},
    env,
    future::Future,
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
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::{
    ApiError, ApiResult, App,
    auth::authenticate_read,
    channels, messages, payload,
    roles::{self, Permission},
};

mod enforcer;
pub mod moderation;

pub use enforcer::{enforce, enforce_person, start as start_enforcer};

const DEV_SECRET: &str = "dev-only-livekit-secret-do-not-deploy";
const DATA_PREFIX: &str = "file-";
const ROSTER_TTL: Duration = Duration::from_secs(1);
/// Clients close their event socket on frames over 4 KiB, so a bigger roster is left out;
/// the margin keeps a notice safe whatever else that frame limit counts.
const MAX_EVENT_BYTES: usize = 3800;

/// LiveKit identities active in a room.
type Active = BTreeSet<String>;
/// A roster fetch in progress; `None` until it ends, then its answer or error code.
type Fetching = watch::Receiver<Option<Result<Active, &'static str>>>;

#[derive(PartialEq)]
enum Outcome {
    Applied,
    AlreadyAbsent,
}

pub struct LiveKit {
    api_key: String,
    api_secret: Zeroizing<String>,
    webhook: WebhookReceiver,
    http: reqwest::Client,
    /// Active identities per room and when they were asked for; `None` marks a room
    /// LiveKit reported a change in at that time, so older answers are not used again.
    rosters: Mutex<HashMap<String, (Instant, Option<Active>)>>,
    /// One roster fetch per room at a time; concurrent askers wait for its answer.
    fetching: Mutex<HashMap<String, Fetching>>,
    /// Access checks waiting for the enforcement worker.
    queue: enforcer::Queue,
}

/// Removes a finished or dropped fetch, unless a change already replaced it.
struct Done<'a> {
    fetching: &'a Mutex<HashMap<String, Fetching>>,
    room: &'a str,
    answer: Fetching,
}

impl Drop for Done<'_> {
    fn drop(&mut self) {
        let mut fetching = self.fetching.lock().expect("roster fetches");
        if fetching
            .get(self.room)
            .is_some_and(|answer| answer.same_channel(&self.answer))
        {
            fetching.remove(self.room);
        }
    }
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
            fetching: Mutex::default(),
            queue: Default::default(),
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

    async fn active(&self, room: &str) -> ApiResult<Active> {
        self.active_with(room, || async {
            Ok(self
                .list(room)
                .await?
                .into_iter()
                .filter(|participant| participant.state == "ACTIVE")
                .map(|participant| participant.identity)
                .collect())
        })
        .await
    }

    /// The cached roster, or one `fetch` shared by everyone asking for `room` meanwhile,
    /// so a join or leave that every client reacts to costs LiveKit one call.
    async fn active_with<F, Fut>(&self, room: &str, mut fetch: F) -> ApiResult<Active>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = ApiResult<Active>>,
    {
        loop {
            let (mut answer, lead) = {
                // Always `rosters` before `fetching`, as in `changed`.
                let rosters = self.rosters.lock().expect("roster cache");
                if let Some((at, Some(active))) = rosters.get(room)
                    && at.elapsed() < ROSTER_TTL
                {
                    return Ok(active.clone());
                }
                let mut fetching = self.fetching.lock().expect("roster fetches");
                match fetching.get(room) {
                    Some(answer) => (answer.clone(), None),
                    None => {
                        let (lead, answer) = watch::channel(None);
                        fetching.insert(room.into(), answer.clone());
                        (answer, Some(lead))
                    }
                }
            };
            let Some(lead) = lead else {
                // A dropped leader closes the channel without an answer; then ask again.
                let result = answer
                    .wait_for(Option::is_some)
                    .await
                    .ok()
                    .and_then(|r| r.clone());
                match result {
                    Some(result) => {
                        return result.map_err(|code| ApiError::new(StatusCode::BAD_GATEWAY, code));
                    }
                    None => continue,
                }
            };
            // Ends this fetch even if the caller is dropped, so waiters ask again.
            let _done = Done {
                fetching: &self.fetching,
                room,
                answer,
            };
            let asked = Instant::now();
            let result = fetch().await;
            if let Ok(active) = &result {
                self.remember(room, asked, active.clone());
            }
            lead.send_replace(Some(result.as_ref().cloned().map_err(|error| error.code)));
            return result;
        }
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
        // A fetch sent before the change must not answer askers who come after it.
        self.fetching.lock().expect("roster fetches").remove(room);
    }

    async fn try_moderate(
        &self,
        room: &str,
        method: &str,
        body: serde_json::Value,
    ) -> ApiResult<Outcome> {
        match self.room_service(method, room, body).await? {
            response if response.status().is_success() => {
                tracing::info!(method, "changed voice room");
                Ok(Outcome::Applied)
            }
            // The participant left, or the room closed, on its own in the meantime.
            response if response.status() == StatusCode::NOT_FOUND => Ok(Outcome::AlreadyAbsent),
            _ => Err(ApiError::new(StatusCode::BAD_GATEWAY, "livekit_rejected")),
        }
    }

    /// Like `try_moderate`, but logs a failure; returns whether the change went through.
    async fn moderate(&self, room: &str, method: &str, body: serde_json::Value) -> bool {
        let result = self.try_moderate(room, method, body).await;
        if let Err(error) = &result {
            tracing::warn!(method, reason = error.code, "could not change voice room");
        }
        result.is_ok()
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
    let (audio_kbps, member_limit): (i64, i64) =
        query_as("SELECT audio_kbps, member_limit FROM channels WHERE id = ?")
            .bind(&channel)
            .fetch_one(&mut *tx)
            .await?;
    // Channel managers may raise the limit anyway, so they also get into a full channel.
    let capped = member_limit > 0
        && !roles::authority(&mut tx, &actor.person)
            .await?
            .has(Permission::ManageChannels);
    let readers = if capped {
        reader_identities(&app, &mut tx, &channel).await?
    } else {
        HashMap::new()
    };
    tx.commit().await?;
    let livekit = configured(&app)?;
    if capped {
        let active = livekit.active(&channel).await?;
        if others(&active, &readers, &actor.person) >= member_limit as usize {
            return Err(ApiError::forbidden("voice_channel_full"));
        }
    }
    let identity = voice_identity(app.key.verifying_key().as_bytes(), &actor.device);
    let token = livekit.sign(
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
    Ok(Json(Token {
        token,
        audio_kbps: Some(audio_kbps),
    }))
}

/// How many people besides `person` are in the call. A rejoin or second device of
/// `person` takes no extra place; an identity no reader owns counts on its own.
fn others(active: &Active, readers: &HashMap<String, Vec<u8>>, person: &[u8]) -> usize {
    active
        .iter()
        .filter_map(|identity| match readers.get(identity) {
            Some(owner) if owner == person => None,
            Some(owner) => Some(owner.as_slice()),
            None => Some(identity.as_bytes()),
        })
        .collect::<BTreeSet<_>>()
        .len()
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
        // A moderator's mute silences them fully: app audio could carry the microphone too.
        if !moderation::muted(db, person).await? {
            sources.extend([TrackSource::Microphone, TrackSource::ScreenShareAudio]);
        }
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
    Ok(Json(presence(&active, &readers)))
}

/// Who of `active` the channel's readers are, by person.
fn presence(active: &Active, readers: &HashMap<String, Vec<u8>>) -> Presence {
    let mut presence = Presence {
        people: BTreeSet::new(),
        unknown: 0,
    };
    for identity in active {
        match readers.get(identity) {
            Some(person) => {
                presence.people.insert(encode(person));
            }
            None => presence.unknown += 1,
        }
    }
    presence
}

/// Tell the channel's readers its roster changed, with the new roster when it is known
/// to be from after the change, so they need not each ask LiveKit again.
async fn announce(app: &App, livekit: &LiveKit, channel: String) {
    let readers = async {
        livekit.active(&channel).await?;
        let mut db = app.db.acquire().await?;
        reader_identities(app, &mut db, &channel).await
    }
    .await;
    // Under the roster lock, so a later change is never announced before this one.
    let rosters = livekit.rosters.lock().expect("roster cache");
    let roster = match readers {
        Err(error) => {
            tracing::warn!(reason = error.code, "could not include the voice roster");
            None
        }
        // No fresh roster means another change came in meanwhile; clients then ask.
        Ok(readers) => match rosters.get(&channel) {
            Some((at, Some(active))) if at.elapsed() < ROSTER_TTL => {
                Some(presence(active, &readers))
            }
            _ => None,
        },
    };
    app.notify(voice_changed(channel, roster));
    drop(rosters);
}

/// The notice for a changed voice channel, with `roster` if it fits in one frame:
/// clients close their socket on a larger one, so they ask for a big roster instead.
fn voice_changed(channel: String, roster: Option<Presence>) -> Event {
    let (people, unknown) = roster
        .map(|found| (found.people.into_iter().collect(), found.unknown))
        .unzip();
    let full = Event::VoiceChanged {
        channel: channel.clone(),
        people,
        unknown,
    };
    let fits = serde_json::to_vec(&rusttalk_core::presence::Update::Event(full.clone()))
        .is_ok_and(|json| json.len() <= MAX_EVENT_BYTES);
    if fits {
        full
    } else {
        Event::VoiceChanged {
            channel,
            people: None,
            unknown: None,
        }
    }
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

enum Action {
    Keep,
    Remove,
    Update(ParticipantPermission),
}

/// What a participant may do now, from the database as it is at this moment
/// and `readers` from `reader_identities` for this check.
async fn decide(
    app: &App,
    channel: &str,
    data: bool,
    participant: &Participant,
    readers: &HashMap<String, Vec<u8>>,
) -> ApiResult<Action> {
    let mut db = app.db.acquire().await?;
    let device = if data {
        (participant.identity.split_once('.')).map_or("", |(device, _)| device)
    } else {
        &participant.identity
    };
    let Some(person) = readers.get(device) else {
        return Ok(Action::Remove);
    };
    if data {
        // Readers stay to receive; a sender that lost write leaves.
        let kind: Option<String> = query_scalar("SELECT kind FROM channels WHERE id = ?")
            .bind(channel)
            .fetch_optional(&mut *db)
            .await?;
        let lost = participant.permission.can_publish_data
            && !messages::can_write(&mut db, channel, person, kind.as_deref().unwrap_or(""))
                .await?;
        return Ok(if lost { Action::Remove } else { Action::Keep });
    }
    // Grants follow the current flags both ways, so lifting a mute gives the microphone back.
    let mut permission = participant.permission.clone();
    permission.can_publish_data =
        channels::has_flag(&mut db, channel, person, channels::WRITE).await?;
    let mut allowed: Vec<i32> = (publish_sources(&mut db, channel, person).await?.into_iter())
        .map(|source| source as i32)
        .collect();
    allowed.sort_unstable();
    permission.can_publish = !allowed.is_empty();
    permission.can_publish_sources = allowed;
    let mut granted = participant.permission.can_publish_sources.clone();
    granted.sort_unstable();
    let same = granted == permission.can_publish_sources
        && participant.permission.can_publish == permission.can_publish
        && participant.permission.can_publish_data == permission.can_publish_data;
    Ok(if same {
        Action::Keep
    } else {
        Action::Update(permission)
    })
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
            // Not tied to this request, which LiveKit may drop before the notice goes out.
            let app = app.clone();
            tokio::spawn(async move {
                if let Some(livekit) = app.voice.clone() {
                    announce(&app, &livekit, room.name).await;
                }
            });
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
            limits: crate::limits::Limits::for_tests(None, true).into(),
            voice: Some(Arc::new(LiveKit {
                api_key: api_key.into(),
                api_secret: Zeroizing::new(api_secret.into()),
                webhook: WebhookReceiver::new(TokenVerifier::with_api_key(api_key, api_secret)),
                http: reqwest::Client::new(),
                rosters: Mutex::default(),
                fetching: Mutex::default(),
                queue: Default::default(),
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
    fn a_full_channel_counts_people_not_devices() {
        let readers = HashMap::from([
            ("ann-phone".to_owned(), vec![1]),
            ("ann-laptop".to_owned(), vec![1]),
            ("bob".to_owned(), vec![2]),
        ]);
        let active: Active = ["ann-phone", "ann-laptop", "bob", "stranger"]
            .map(String::from)
            .into();
        // Ann's two devices are one person; the unknown identity takes its own place.
        assert_eq!(others(&active, &readers, &[3]), 3);
        // Ann rejoining or adding a device takes no extra place.
        assert_eq!(others(&active, &readers, &[1]), 2);
        assert_eq!(others(&Active::new(), &readers, &[3]), 0);
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
        let livekit = livekit();
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

    fn livekit() -> LiveKit {
        let secret = "test-secret-with-at-least-32-characters";
        LiveKit {
            api_key: "test-key".into(),
            api_secret: Zeroizing::new(secret.into()),
            webhook: WebhookReceiver::new(TokenVerifier::with_api_key("test-key", secret)),
            http: reqwest::Client::new(),
            rosters: Mutex::default(),
            fetching: Mutex::default(),
            queue: Default::default(),
        }
    }

    /// Starts a fetch for "room" that waits for `release`, and waits until it is running.
    async fn held_fetch(
        livekit: &Arc<LiveKit>,
        calls: &Arc<std::sync::atomic::AtomicUsize>,
        result: ApiResult<Active>,
    ) -> (
        tokio::task::JoinHandle<ApiResult<Active>>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (release, released) = tokio::sync::oneshot::channel();
        let (started, running) = tokio::sync::oneshot::channel();
        let mut once = Some((started, released, result));
        let (livekit, calls) = (livekit.clone(), calls.clone());
        let leader = tokio::spawn(async move {
            livekit
                .active_with("room", || {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let (started, released, result) = once.take().expect("one fetch");
                    async move {
                        let _ = started.send(());
                        let _ = released.await;
                        result
                    }
                })
                .await
        });
        running.await.expect("fetch started");
        (leader, release)
    }

    /// Asks for "room" with a fetch that answers `bob` at once.
    fn ask(
        livekit: &Arc<LiveKit>,
        calls: &Arc<std::sync::atomic::AtomicUsize>,
    ) -> tokio::task::JoinHandle<ApiResult<Active>> {
        let (livekit, calls) = (livekit.clone(), calls.clone());
        tokio::spawn(async move {
            livekit
                .active_with("room", || {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async { Ok(BTreeSet::from(["bob".to_owned()])) }
                })
                .await
        })
    }

    #[tokio::test]
    async fn a_join_storm_asks_livekit_once() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        let (livekit, calls) = (Arc::new(livekit()), Arc::default());
        let alice = BTreeSet::from(["alice".to_owned()]);
        let (leader, release) = held_fetch(&livekit, &calls, Ok(alice.clone())).await;
        let waiters: Vec<_> = (0..5).map(|_| ask(&livekit, &calls)).collect();
        tokio::task::yield_now().await;
        release.send(()).ok();
        assert_eq!(leader.await?.unwrap(), alice);
        for waiter in waiters {
            assert_eq!(waiter.await?.unwrap(), alice);
        }
        assert_eq!(calls.load(SeqCst), 1);

        // After a reported change, nobody gets the answer from before it.
        livekit.changed("room");
        let (leader, release) = held_fetch(&livekit, &calls, Ok(alice.clone())).await;
        livekit.changed("room");
        let after = ask(&livekit, &calls).await?.unwrap();
        assert_eq!(after, BTreeSet::from(["bob".to_owned()]));
        release.send(()).ok();
        assert_eq!(leader.await?.unwrap(), alice);
        // The late answer is not cached over the newer one.
        assert_eq!(ask(&livekit, &calls).await?.unwrap(), after);
        assert_eq!(calls.load(SeqCst), 3);
        Ok(())
    }

    #[tokio::test]
    async fn waiters_get_a_failed_fetch_and_outlive_a_dropped_one() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        let (livekit, calls) = (Arc::new(livekit()), Arc::default());
        let down = || ApiError::new(StatusCode::BAD_GATEWAY, "livekit_roster_unavailable");
        let (leader, release) = held_fetch(&livekit, &calls, Err(down())).await;
        let waiter = ask(&livekit, &calls);
        tokio::task::yield_now().await;
        release.send(()).ok();
        assert_eq!(
            leader.await?.unwrap_err().code,
            "livekit_roster_unavailable"
        );
        assert_eq!(
            waiter.await?.unwrap_err().code,
            "livekit_roster_unavailable"
        );
        assert_eq!(calls.load(SeqCst), 1);

        // A leader whose request is dropped mid-fetch hands over instead of hanging waiters.
        let (leader, _release) = held_fetch(&livekit, &calls, Err(down())).await;
        let waiter = ask(&livekit, &calls);
        tokio::task::yield_now().await;
        leader.abort();
        let answer = tokio::time::timeout(Duration::from_secs(5), waiter).await??;
        assert_eq!(answer.unwrap(), BTreeSet::from(["bob".to_owned()]));
        assert_eq!(calls.load(SeqCst), 3);
        Ok(())
    }

    #[test]
    fn a_roster_too_big_for_one_frame_is_left_out() {
        let roster = |count: usize| Presence {
            people: (0..count).map(|i| encode(&[i as u8; 32])).collect(),
            unknown: 1,
        };
        let inline = |event: Event| match event {
            Event::VoiceChanged {
                people, unknown, ..
            } => people.map(|people| people.len()).zip(unknown),
            _ => None,
        };
        let channel = encode(&[9; 32]);
        assert_eq!(
            inline(voice_changed(channel.clone(), Some(roster(3)))),
            Some((3, 1))
        );
        assert_eq!(
            inline(voice_changed(channel.clone(), Some(roster(200)))),
            None
        );
        assert_eq!(inline(voice_changed(channel, None)), None);
    }
}
