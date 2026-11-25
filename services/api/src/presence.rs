use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
    time::Duration,
};

use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use rusttalk_core::{
    auth::encode,
    presence::{Presence, PresenceEvent, Status, Update},
};
use serde::Deserialize;
use sqlx::{SqliteConnection, query, query_scalar};
use tokio::time::{Instant, sleep_until};

use crate::{
    ApiResult, App, Change,
    auth::{authenticate, authenticate_read},
    payload,
};

const RECONNECT_GRACE: Duration = Duration::from_secs(1);
/// At most one presence notice per member per interval; later choices are coalesced.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Sessions(Mutex<Known>);

#[derive(Default)]
struct Known {
    members: HashMap<String, Member>,
    /// Choices made since start. A socket that read the stored choice just before a
    /// change was saved would otherwise announce the old one until the next change.
    chosen: HashMap<String, Status>,
    /// Devices that reported no input for a while, kept in memory only. An entry goes
    /// once its device has been disconnected for the reconnect grace.
    idle: HashSet<String>,
}

struct Member {
    /// Open event sockets per device ID.
    devices: HashMap<String, usize>,
    status: Status,
    closes_at: Option<Instant>,
    /// What other members were last told, so every notice is a real change.
    announced: Presence,
    announced_at: Instant,
    flush_pending: bool,
}

/// What others see: the account-wide choice, where "Online" shows as away while every
/// connected device is idle, like Discord.
fn visible(member: &Member, idle: &HashSet<String>) -> Presence {
    match member.status {
        Status::Online if member.devices.keys().all(|device| idle.contains(device)) => {
            Presence::Away
        }
        Status::Online => Presence::Online,
        Status::Away => Presence::Away,
        Status::Invisible => Presence::Offline,
    }
}

#[derive(Debug, PartialEq)]
enum Announce {
    Nothing,
    Now(Presence),
    At(Instant),
}

/// Decide whether a presence change is told now, later, or not at all. Without a
/// connected device the member is within the reconnect grace: nothing to tell yet.
fn announcement(member: &Member, idle: &HashSet<String>, now: Instant) -> Announce {
    let current = visible(member, idle);
    if current == member.announced || member.devices.is_empty() {
        Announce::Nothing
    } else if now >= member.announced_at + ANNOUNCE_INTERVAL {
        Announce::Now(current)
    } else if member.flush_pending {
        Announce::Nothing
    } else {
        Announce::At(member.announced_at + ANNOUNCE_INTERVAL)
    }
}

/// Apply the announcement decision for one member; the caller holds the lock.
fn announce(app: &App, person_id: &str, member: &mut Member, idle: &HashSet<String>) {
    let now = Instant::now();
    match announcement(member, idle, now) {
        Announce::Nothing => {}
        Announce::Now(presence) => {
            member.announced = presence;
            member.announced_at = now;
            changed(app, person_id, presence);
        }
        Announce::At(at) => {
            member.flush_pending = true;
            let app = app.clone();
            let person_id = person_id.to_owned();
            tokio::spawn(async move {
                sleep_until(at).await;
                let mut state = app.presence.0.lock().expect("presence sessions");
                let Known { members, idle, .. } = &mut *state;
                if let Some(member) = members.get_mut(&person_id) {
                    member.flush_pending = false;
                    announce(&app, &person_id, member, idle);
                }
            });
        }
    }
}

impl Sessions {
    pub fn snapshot(&self) -> HashMap<String, Presence> {
        self.0
            .lock()
            .expect("presence sessions")
            .members
            .iter()
            .map(|(id, member)| (id.clone(), member.announced))
            .collect()
    }
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Online => "online",
        Status::Away => "away",
        Status::Invisible => "invisible",
    }
}

/// The member's stored choice. Only the current choice is kept, never a history.
pub async fn stored(db: &mut SqliteConnection, person: &[u8]) -> ApiResult<Status> {
    let name: Option<String> =
        query_scalar("SELECT presence_status FROM members WHERE person_id = ?")
            .bind(person)
            .fetch_optional(db)
            .await?;
    Ok(match name.as_deref() {
        Some("away") => Status::Away,
        Some("invisible") => Status::Invisible,
        _ => Status::Online,
    })
}

pub struct Socket {
    app: App,
    person_id: String,
    device: String,
}

/// Count an authenticated event socket of `device`, starting from the stored choice so
/// an invisible member is never announced online on connect.
pub fn open(app: &App, person_id: String, device: String, status: Status) -> Socket {
    let mut state = app.presence.0.lock().expect("presence sessions");
    let state = &mut *state;
    let status = state.chosen.get(&person_id).copied().unwrap_or(status);
    let fresh = !state.members.contains_key(&person_id);
    let member = state
        .members
        .entry(person_id.clone())
        .or_insert_with(|| Member {
            devices: HashMap::new(),
            status,
            closes_at: None,
            announced: Presence::Offline,
            announced_at: Instant::now(),
            flush_pending: false,
        });
    *member.devices.entry(device.clone()).or_default() += 1;
    member.closes_at = None;
    if fresh {
        member.announced = visible(member, &state.idle);
        if member.announced != Presence::Offline {
            changed(app, &person_id, member.announced);
        }
    } else {
        // A new, active device can end "away" at once.
        announce(app, &person_id, member, &state.idle);
    }
    Socket {
        app: app.clone(),
        person_id,
        device,
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let app = self.app.clone();
        let (person_id, device) = (self.person_id.clone(), self.device.clone());
        let mut state = app.presence.0.lock().expect("presence sessions");
        let Known { members, idle, .. } = &mut *state;
        let Some(member) = members.get_mut(&person_id) else {
            return;
        };
        let Some(count) = member.devices.get_mut(&device) else {
            return;
        };
        *count -= 1;
        if *count != 0 {
            return;
        }
        // This device no longer counts; the others may all be idle.
        member.devices.remove(&device);
        let closes_at = Instant::now() + RECONNECT_GRACE;
        if member.devices.is_empty() {
            member.closes_at = Some(closes_at);
        } else {
            announce(&app, &person_id, member, idle);
        }
        drop(state);
        tokio::spawn(async move {
            sleep_until(closes_at).await;
            let mut state = app.presence.0.lock().expect("presence sessions");
            let member = state.members.get(&person_id);
            let gone = member.is_none_or(|member| !member.devices.contains_key(&device));
            let closes = member.is_some_and(|member| member.closes_at == Some(closes_at));
            // Disconnected for the whole grace: forget whether this device was idle.
            if gone {
                state.idle.remove(&device);
            }
            if closes
                && let Some(member) = state.members.remove(&person_id)
                && member.announced != Presence::Offline
            {
                changed(&app, &person_id, Presence::Offline);
            }
        });
    }
}

fn changed(app: &App, person_id: &str, status: Presence) {
    let _ = app.changes.send(Change::update(Update::Presence(
        PresenceEvent::PresenceChanged {
            person_id: person_id.to_owned(),
            status,
            chosen: None,
        },
    )));
}

#[derive(Deserialize)]
pub struct Choice {
    status: Status,
    /// Sent by apps that report idle separately (`PUT /v1/me/idle`), so every status
    /// here was picked by the person.
    #[serde(default)]
    picked: bool,
}

pub async fn set(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Choice>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Choice { status, picked } = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    let (person, device) = (encode(&actor.person), encode(&actor.device));
    // Older apps send "away" by themselves after a while without input and "online"
    // once used again, so their "away" counts as that device being idle.
    if !picked && status == Status::Away {
        drop(tx);
        set_idle(&app, &person, device, true);
        return Ok(StatusCode::NO_CONTENT);
    }
    // Kept so a reconnect, restart or another device starts from the same choice.
    query("UPDATE members SET presence_status = ? WHERE person_id = ?")
        .bind(status_name(status))
        .bind(actor.person.as_slice())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    apply(&app, &person, status);
    if !picked && status == Status::Online {
        set_idle(&app, &person, device, false);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Idle {
    idle: bool,
}

/// This device has had no input for a while, or is used again. Not stored: a choice of
/// "Online" shows as away only while every connected device of the member is idle.
pub async fn idle(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Idle>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Idle { idle } = payload(body)?;
    let (tx, actor) = authenticate_read(&app, &headers).await?;
    drop(tx);
    set_idle(&app, &encode(&actor.person), encode(&actor.device), idle);
    Ok(StatusCode::NO_CONTENT)
}

/// Kept also before the device's socket is counted, so a report racing a reconnect holds.
fn set_idle(app: &App, person_id: &str, device: String, idle: bool) {
    let mut state = app.presence.0.lock().expect("presence sessions");
    let state = &mut *state;
    let changed = if idle {
        state.idle.insert(device)
    } else {
        state.idle.remove(&device)
    };
    if changed && let Some(member) = state.members.get_mut(person_id) {
        announce(app, person_id, member, &state.idle);
    }
}

#[derive(Deserialize)]
pub struct HideOffline {
    hide_offline: bool,
}

/// Leave this member out of other people's member lists while they appear offline.
pub async fn set_hide_offline(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<HideOffline>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let HideOffline { hide_offline } = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    query("UPDATE members SET hide_offline = ? WHERE person_id = ?")
        .bind(hide_offline)
        .bind(actor.person.as_slice())
        .execute(&mut *tx)
        .await?;
    let profile = crate::metadata::profile(&mut tx, &actor.person).await?;
    tx.commit().await?;
    // Others update this member, whose profile carries the flag.
    crate::keys::profile_changed(&app, &actor.person, profile);
    Ok(StatusCode::NO_CONTENT)
}

/// Apply a choice to a connected member, including one within the reconnect grace,
/// so the reconnected sockets keep the new choice, and remember it for the next socket.
/// The member's own devices hear the choice at once, so all of them show it.
fn apply(app: &App, person_id: &str, status: Status) {
    let mut state = app.presence.0.lock().expect("presence sessions");
    let state = &mut *state;
    state.chosen.insert(person_id.to_owned(), status);
    if let Some(member) = state.members.get_mut(person_id) {
        member.status = status;
        announce(app, person_id, member, &state.idle);
        let _ = app.changes.send(Change::update(Update::Presence(
            PresenceEvent::PresenceChanged {
                person_id: person_id.to_owned(),
                // What the others were told, so every device shows the same.
                status: member.announced,
                chosen: Some(status),
            },
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(status: Status, announced: Presence, announced_at: Instant) -> Member {
        Member {
            devices: HashMap::from([("d".to_owned(), 1)]),
            status,
            closes_at: None,
            announced,
            announced_at,
            flush_pending: false,
        }
    }

    #[test]
    fn online_shows_away_only_while_every_device_is_idle() {
        let mut both = member(Status::Online, Presence::Online, Instant::now());
        both.devices.insert("phone".into(), 1);
        let none = HashSet::new();
        let phone = HashSet::from(["phone".to_owned()]);
        let all = HashSet::from(["phone".to_owned(), "d".to_owned()]);
        assert_eq!(visible(&both, &none), Presence::Online);
        assert_eq!(visible(&both, &phone), Presence::Online);
        assert_eq!(visible(&both, &all), Presence::Away);
        // A picked status is the same on every device, idle or not.
        for (status, shown) in [
            (Status::Away, Presence::Away),
            (Status::Invisible, Presence::Offline),
        ] {
            both.status = status;
            for idle in [&none, &phone, &all] {
                assert_eq!(visible(&both, idle), shown);
            }
        }
    }

    #[test]
    fn rapid_status_changes_are_coalesced() {
        let start = Instant::now();
        let soon = start + ANNOUNCE_INTERVAL / 2;
        let later = start + ANNOUNCE_INTERVAL;
        let idle = HashSet::new();

        let mut away = member(Status::Away, Presence::Online, start);
        assert_eq!(
            announcement(&away, &idle, soon),
            Announce::At(start + ANNOUNCE_INTERVAL)
        );
        away.flush_pending = true;
        assert_eq!(announcement(&away, &idle, soon), Announce::Nothing);
        assert_eq!(
            announcement(&away, &idle, later),
            Announce::Now(Presence::Away)
        );

        // Flipping back before the flush leaves nothing to tell.
        let back = member(Status::Online, Presence::Online, start);
        assert_eq!(announcement(&back, &idle, soon), Announce::Nothing);
    }

    async fn app(dir: &std::path::Path) -> App {
        crate::testing::app(dir, 1).await.unwrap()
    }

    fn shown(app: &App, person: &str) -> Presence {
        app.presence.snapshot()[person]
    }

    /// Past the announce interval, with any announcement or grace due by then done.
    async fn later() {
        tokio::time::sleep(ANNOUNCE_INTERVAL + Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn a_choice_during_the_reconnect_grace_applies() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await;
        drop(open(&app, "p".into(), "d".into(), Status::Online));
        apply(&app, "p", Status::Away);
        let _socket = open(&app, "p".into(), "d".into(), Status::Online);
        let state = app.presence.0.lock().unwrap();
        assert_eq!(state.members["p"].devices["d"], 1);
        assert_eq!(status_name(state.members["p"].status), "away");
    }

    #[tokio::test]
    async fn a_choice_saved_while_a_socket_opens_wins_over_the_stale_read() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await;
        // The socket read "away" from the database, then "online" was saved and
        // applied before the socket was counted.
        apply(&app, "p", Status::Online);
        let _socket = open(&app, "p".into(), "d".into(), Status::Away);
        assert_eq!(shown(&app, "p"), Presence::Online);
    }

    #[tokio::test]
    async fn an_active_device_keeps_the_member_online() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await;
        // Paused once the database is open: its pool would time out at once.
        tokio::time::pause();
        let _desktop = open(&app, "p".into(), "desktop".into(), Status::Online);
        let phone = open(&app, "p".into(), "phone".into(), Status::Online);
        set_idle(&app, "p", "phone".into(), true);
        assert_eq!(shown(&app, "p"), Presence::Online);

        later().await;
        set_idle(&app, "p", "desktop".into(), true);
        assert_eq!(shown(&app, "p"), Presence::Away);

        // Input on any device: online again at once.
        later().await;
        set_idle(&app, "p", "phone".into(), false);
        assert_eq!(shown(&app, "p"), Presence::Online);

        // The active phone leaves: the idle desktop alone shows away.
        later().await;
        drop(phone);
        assert_eq!(shown(&app, "p"), Presence::Away);
    }

    #[tokio::test]
    async fn a_picked_status_applies_to_every_device() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await;
        // Paused once the database is open: its pool would time out at once.
        tokio::time::pause();
        let mut changes = app.changes.subscribe();
        let _desktop = open(&app, "p".into(), "desktop".into(), Status::Online);
        let _phone = open(&app, "p".into(), "phone".into(), Status::Online);
        set_idle(&app, "p", "phone".into(), true);
        for (status, presence) in [
            (Status::Away, Presence::Away),
            (Status::Invisible, Presence::Offline),
            (Status::Online, Presence::Online),
        ] {
            later().await;
            apply(&app, "p", status);
            assert_eq!(shown(&app, "p"), presence);
        }
        // The member's own devices hear each choice, so all of them show it.
        let mut chosen = Vec::new();
        while let Ok(Change::Update(notice)) = changes.try_recv() {
            if let Update::Presence(PresenceEvent::PresenceChanged {
                chosen: Some(status),
                ..
            }) = notice.update
            {
                chosen.push(status);
            }
        }
        assert_eq!(chosen, [Status::Away, Status::Invisible, Status::Online]);
    }

    #[tokio::test]
    async fn idle_holds_through_a_reconnect_and_goes_with_the_device() {
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await;
        // Paused once the database is open: its pool would time out at once.
        tokio::time::pause();
        let _desktop = open(&app, "p".into(), "desktop".into(), Status::Online);
        set_idle(&app, "p", "desktop".into(), true);
        drop(open(&app, "p".into(), "phone".into(), Status::Online));
        // Reported while reconnecting, before the new socket is counted.
        set_idle(&app, "p", "phone".into(), true);
        let phone = open(&app, "p".into(), "phone".into(), Status::Online);
        later().await;
        assert_eq!(shown(&app, "p"), Presence::Away);
        // Gone for longer than the grace: the phone starts active when it connects again.
        drop(phone);
        later().await;
        later().await;
        assert!(!app.presence.0.lock().unwrap().idle.contains("phone"));
        let _phone = open(&app, "p".into(), "phone".into(), Status::Online);
        assert_eq!(shown(&app, "p"), Presence::Online);
    }

    #[tokio::test]
    async fn an_older_apps_away_counts_as_that_device_idle() {
        // Real time: the handler uses the database, whose pool times out in paused time.
        let dir = tempfile::tempdir().unwrap();
        let app = app(dir.path()).await;
        let person = encode(&crate::testing::person(1));
        // The test session's device; the newer app is another device.
        let _old = open(&app, person.clone(), encode(&[1]), Status::Online);
        let _new = open(&app, person.clone(), "new".into(), Status::Online);
        let put = |json| {
            let choice = serde_json::from_value(json).unwrap();
            set(
                State(app.clone()),
                crate::testing::as_person(1),
                Ok(Json(choice)),
            )
        };
        let choice = async || {
            let mut db = app.db.acquire().await.unwrap();
            stored(&mut db, &crate::testing::person(1)).await.unwrap()
        };

        put(serde_json::json!({"status": "away"})).await.unwrap();
        assert_eq!(shown(&app, &person), Presence::Online);
        set_idle(&app, &person, "new".into(), true);
        later().await;
        assert_eq!(shown(&app, &person), Presence::Away);
        assert_eq!(choice().await, Status::Online, "not stored as a choice");

        // Its "online" is a choice and ends its idle.
        later().await;
        put(serde_json::json!({"status": "online"})).await.unwrap();
        assert_eq!(shown(&app, &person), Presence::Online);

        // A newer app's picked "away" is stored.
        put(serde_json::json!({"status": "away", "picked": true}))
            .await
            .unwrap();
        assert_eq!(choice().await, Status::Away);
    }

    #[test]
    fn invisible_is_announced_as_offline() {
        let start = Instant::now();
        let idle = HashSet::new();
        let hidden = member(Status::Invisible, Presence::Online, start);
        assert_eq!(
            announcement(&hidden, &idle, start + ANNOUNCE_INTERVAL),
            Announce::Now(Presence::Offline)
        );
        let already = member(Status::Invisible, Presence::Offline, start);
        assert_eq!(
            announcement(&already, &idle, start + ANNOUNCE_INTERVAL),
            Announce::Nothing
        );
    }
}
