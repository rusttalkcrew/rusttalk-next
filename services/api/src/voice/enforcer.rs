//! One worker applies access changes to LiveKit, one at a time and in order, so a decision
//! made before a later change never lands after it. Requests coalesce instead of piling up:
//! a burst of joins in one room is one pending room check, and a mute change goes first.

use std::{collections::BTreeSet, time::Duration};

use rusttalk_core::auth::voice_identity;
use serde_json::json;
use sqlx::query_scalar;

use super::{Action, DATA_PREFIX, LiveKit, decide, room_channel};
use crate::{ApiResult, App};

#[derive(Default)]
pub(super) struct Pending {
    /// People whose voice mute changed, checked first.
    persons: BTreeSet<Vec<u8>>,
    /// Every room, as after a membership or role change.
    full: bool,
    /// Voice channels whose rooms to check, as after a join.
    rooms: BTreeSet<String>,
}

#[derive(Debug, PartialEq)]
enum Job {
    Persons(BTreeSet<Vec<u8>>),
    Full,
    Room(String),
}

impl Pending {
    fn next(&mut self) -> Option<Job> {
        if !self.persons.is_empty() {
            return Some(Job::Persons(std::mem::take(&mut self.persons)));
        }
        if std::mem::take(&mut self.full) {
            // A full pass covers the rooms queued so far.
            self.rooms.clear();
            return Some(Job::Full);
        }
        self.rooms.pop_first().map(Job::Room)
    }

    fn put_back(&mut self, job: Job) {
        match job {
            Job::Persons(persons) => self.persons.extend(persons),
            Job::Full => self.full = true,
            Job::Room(room) => {
                self.rooms.insert(room);
            }
        }
    }
}

fn queue(app: &App, add: impl FnOnce(&mut Pending)) {
    let Some(livekit) = app.voice.as_deref() else {
        return;
    };
    add(&mut livekit.pending.lock().expect("pending voice checks"));
    livekit.wake.notify_one();
}

/// Disconnect devices that lost read access and set publication grants to what is allowed
/// now, in one room or (`None`) all. Runs after the change commits and never blocks it;
/// LiveKit keeps the grants a device joined with, and refreshes its token with them, until
/// told otherwise.
pub fn enforce(app: &App, room: Option<String>) {
    queue(app, |pending| match room {
        Some(room) => {
            pending.rooms.insert(room);
        }
        None => pending.full = true,
    });
}

/// Apply a changed voice mute to this person's devices in every room.
pub fn enforce_person(app: &App, person: &[u8]) {
    queue(app, |pending| {
        pending.persons.insert(person.to_vec());
    });
}

/// Start the worker. It first applies what a restart or LiveKit outage left unapplied.
pub fn start(app: &App) {
    let Some(livekit) = app.voice.clone() else {
        return;
    };
    livekit.pending.lock().expect("pending voice checks").full = true;
    let app = app.clone();
    tokio::spawn(async move {
        let mut failures = 0;
        loop {
            let job = livekit.pending.lock().expect("pending voice checks").next();
            let Some(job) = job else {
                livekit.wake.notified().await;
                continue;
            };
            let complete = run(&app, &livekit, &job).await.unwrap_or_else(|error| {
                tracing::warn!(reason = error.code, "could not check voice access");
                false
            });
            if complete {
                failures = 0;
                continue;
            }
            // LiveKit can be briefly unreachable, and lost access must still be taken away.
            livekit
                .pending
                .lock()
                .expect("pending voice checks")
                .put_back(job);
            failures = (failures + 1).min(5);
            tokio::time::sleep(Duration::from_secs(1 << failures)).await;
        }
    });
}

/// Returns whether every room was checked and every change went through.
async fn run(app: &App, livekit: &LiveKit, job: &Job) -> ApiResult<bool> {
    let (rooms, only) = match job {
        Job::Full => (livekit.rooms().await?, None),
        Job::Room(channel) => (
            vec![format!("{DATA_PREFIX}{channel}"), channel.clone()],
            None,
        ),
        Job::Persons(persons) => {
            let mut identities = BTreeSet::new();
            let mut db = app.db.acquire().await?;
            let server = app.key.verifying_key();
            for person in persons {
                let devices: Vec<Vec<u8>> =
                    query_scalar("SELECT device_id FROM member_devices WHERE person_id = ?")
                        .bind(person)
                        .fetch_all(&mut *db)
                        .await?;
                identities.extend(
                    (devices.iter()).map(|device| voice_identity(server.as_bytes(), device)),
                );
            }
            drop(db);
            (livekit.rooms().await?, Some(identities))
        }
    };
    let mut complete = true;
    for room in rooms {
        let Some((channel, data)) = room_channel(&room) else {
            continue;
        };
        let participants = match livekit.list(&room).await {
            Ok(participants) => participants,
            Err(error) => {
                tracing::warn!(reason = error.code, "could not check voice access");
                complete = false;
                continue;
            }
        };
        for participant in participants {
            let device = (participant.identity.split_once('.'))
                .map_or(participant.identity.as_str(), |(device, _)| device);
            if only.as_ref().is_some_and(|only| !only.contains(device)) {
                continue;
            }
            // Decided right before each change, without holding a connection across it.
            let body = match decide(app, channel, data, &participant).await? {
                Action::Keep => continue,
                Action::Remove => {
                    let body = json!({ "room": room, "identity": participant.identity });
                    complete &= livekit.moderate(&room, "RemoveParticipant", body).await;
                    continue;
                }
                Action::Update(permission) => json!({
                    "room": room,
                    "identity": participant.identity,
                    "permission": permission,
                }),
            };
            complete &= livekit.moderate(&room, "UpdateParticipant", body).await;
        }
    }
    Ok(complete)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_join_storm_stays_one_check_and_a_mute_goes_first() {
        let mut pending = Pending::default();
        // A burst of joins in room B while a role change is waiting.
        for _ in 0..10_000 {
            pending.rooms.insert("B".into());
        }
        pending.full = true;
        // Then a mute in room A.
        pending.persons.insert(vec![1]);
        assert_eq!(pending.next(), Some(Job::Persons([vec![1]].into())));
        assert_eq!(pending.next(), Some(Job::Full));
        assert_eq!(pending.next(), None);

        // While a slow LiveKit holds up room B's check, a mute still comes next.
        pending.rooms.extend(["B".into(), "C".into()]);
        let slow = pending.next().unwrap();
        pending.persons.insert(vec![2]);
        pending.put_back(slow);
        assert_eq!(pending.next(), Some(Job::Persons([vec![2]].into())));
        assert_eq!(pending.next(), Some(Job::Room("B".into())));
        assert_eq!(pending.next(), Some(Job::Room("C".into())));
        assert_eq!(pending.next(), None);
    }
}
