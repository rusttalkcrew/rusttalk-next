use std::{collections::HashMap, sync::Mutex, time::Duration};

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

use crate::{ApiResult, App, auth::authenticate, payload};

const RECONNECT_GRACE: Duration = Duration::from_secs(1);
/// At most one presence notice per member per interval; later choices are coalesced.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Sessions(Mutex<HashMap<String, Member>>);

struct Member {
    count: usize,
    status: Status,
    closes_at: Option<Instant>,
    /// What other members were last told, so every notice is a real change.
    announced: Presence,
    announced_at: Instant,
    flush_pending: bool,
}

fn visible(active: bool, status: Status) -> Presence {
    match (active, status) {
        (true, Status::Online) => Presence::Online,
        (true, Status::Away) => Presence::Away,
        _ => Presence::Offline,
    }
}

#[derive(Debug, PartialEq)]
enum Announce {
    Nothing,
    Now(Presence),
    At(Instant),
}

/// Decide whether a status choice is told now, later, or not at all.
fn announcement(member: &Member, now: Instant) -> Announce {
    let current = visible(true, member.status);
    if current == member.announced {
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
fn announce(app: &App, person_id: &str, member: &mut Member) {
    let now = Instant::now();
    match announcement(member, now) {
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
                if let Some(member) = state.get_mut(&person_id) {
                    member.flush_pending = false;
                    announce(&app, &person_id, member);
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
}

/// Count an authenticated event socket, starting from the stored choice so an
/// invisible member is never announced online on connect.
pub fn open(app: &App, person_id: String, status: Status) -> Socket {
    let mut state = app.presence.0.lock().expect("presence sessions");
    let member = state.entry(person_id.clone()).or_insert_with(|| {
        let announced = visible(true, status);
        if announced != Presence::Offline {
            changed(app, &person_id, announced);
        }
        Member {
            count: 0,
            status,
            closes_at: None,
            announced,
            announced_at: Instant::now(),
            flush_pending: false,
        }
    });
    member.count += 1;
    member.closes_at = None;
    Socket {
        app: app.clone(),
        person_id,
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let app = self.app.clone();
        let person_id = self.person_id.clone();
        let mut state = app.presence.0.lock().expect("presence sessions");
        let Some(member) = state.get_mut(&person_id) else {
            return;
        };
        member.count -= 1;
        if member.count != 0 {
            return;
        }
        let closes_at = Instant::now() + RECONNECT_GRACE;
        member.closes_at = Some(closes_at);
        drop(state);
        tokio::spawn(async move {
            sleep_until(closes_at).await;
            let mut state = app.presence.0.lock().expect("presence sessions");
            if state
                .get(&person_id)
                .is_some_and(|member| member.closes_at == Some(closes_at))
                && let Some(member) = state.remove(&person_id)
                && member.announced != Presence::Offline
            {
                changed(&app, &person_id, Presence::Offline);
            }
        });
    }
}

fn changed(app: &App, person_id: &str, status: Presence) {
    let _ = app
        .changes
        .send(Some(Update::Presence(PresenceEvent::PresenceChanged {
            person_id: person_id.to_owned(),
            status,
        })));
}

#[derive(Deserialize)]
pub struct Choice {
    status: Status,
}

pub async fn set(
    State(app): State<App>,
    headers: HeaderMap,
    body: Result<Json<Choice>, JsonRejection>,
) -> ApiResult<StatusCode> {
    let Choice { status } = payload(body)?;
    let (mut tx, actor) = authenticate(&app, &headers).await?;
    // Kept so a reconnect, restart or another device starts from the same choice.
    query("UPDATE members SET presence_status = ? WHERE person_id = ?")
        .bind(status_name(status))
        .bind(actor.person.as_slice())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let person_id = encode(&actor.person);
    let mut state = app.presence.0.lock().expect("presence sessions");
    if let Some(member) = state.get_mut(&person_id).filter(|member| member.count > 0) {
        member.status = status;
        announce(&app, &person_id, member);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_presence_requires_a_session_and_respects_the_choice() {
        assert_eq!(visible(true, Status::Online), Presence::Online);
        assert_eq!(visible(true, Status::Away), Presence::Away);
        assert_eq!(visible(true, Status::Invisible), Presence::Offline);
        for status in [Status::Online, Status::Away, Status::Invisible] {
            assert_eq!(visible(false, status), Presence::Offline);
        }
    }

    fn member(status: Status, announced: Presence, announced_at: Instant) -> Member {
        Member {
            count: 1,
            status,
            closes_at: None,
            announced,
            announced_at,
            flush_pending: false,
        }
    }

    #[test]
    fn rapid_status_changes_are_coalesced() {
        let start = Instant::now();
        let soon = start + ANNOUNCE_INTERVAL / 2;
        let later = start + ANNOUNCE_INTERVAL;

        let mut away = member(Status::Away, Presence::Online, start);
        assert_eq!(
            announcement(&away, soon),
            Announce::At(start + ANNOUNCE_INTERVAL)
        );
        away.flush_pending = true;
        assert_eq!(announcement(&away, soon), Announce::Nothing);
        assert_eq!(announcement(&away, later), Announce::Now(Presence::Away));

        // Flipping back before the flush leaves nothing to tell.
        let back = member(Status::Online, Presence::Online, start);
        assert_eq!(announcement(&back, soon), Announce::Nothing);
    }

    #[test]
    fn invisible_is_announced_as_offline() {
        let start = Instant::now();
        let hidden = member(Status::Invisible, Presence::Online, start);
        assert_eq!(
            announcement(&hidden, start + ANNOUNCE_INTERVAL),
            Announce::Now(Presence::Offline)
        );
        let already = member(Status::Invisible, Presence::Offline, start);
        assert_eq!(
            announcement(&already, start + ANNOUNCE_INTERVAL),
            Announce::Nothing
        );
    }
}
