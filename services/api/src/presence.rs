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
use tokio::time::{Instant, sleep_until};

use crate::{ApiResult, App, auth::authenticate, payload};

const RECONNECT_GRACE: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Sessions(Mutex<HashMap<String, Member>>);

struct Member {
    count: usize,
    status: Status,
    closes_at: Option<Instant>,
}

fn visible(active: bool, status: Status) -> Presence {
    match (active, status) {
        (true, Status::Online) => Presence::Online,
        (true, Status::Away) => Presence::Away,
        _ => Presence::Offline,
    }
}

impl Sessions {
    pub fn snapshot(&self) -> HashMap<String, Presence> {
        self.0
            .lock()
            .expect("presence sessions")
            .iter()
            .map(|(id, member)| (id.clone(), visible(true, member.status)))
            .collect()
    }
}

pub struct Socket {
    app: App,
    person_id: String,
}

pub fn open(app: &App, person_id: String) -> Socket {
    let mut state = app.presence.0.lock().expect("presence sessions");
    let member = state.entry(person_id.clone()).or_insert_with(|| {
        changed(app, &person_id, Presence::Online);
        Member {
            count: 0,
            status: Status::Online,
            closes_at: None,
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
        let member = state.get_mut(&person_id).expect("open presence socket");
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
                && visible(true, member.status) != Presence::Offline
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
    let (tx, actor) = authenticate(&app, &headers).await?;
    tx.commit().await?;
    let person_id = encode(&actor.person);
    let mut state = app.presence.0.lock().expect("presence sessions");
    if let Some(member) = state.get_mut(&person_id).filter(|member| member.count > 0) {
        let before = visible(true, member.status);
        member.status = status;
        let after = visible(true, status);
        if after != before {
            changed(&app, &person_id, after);
        }
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
}
