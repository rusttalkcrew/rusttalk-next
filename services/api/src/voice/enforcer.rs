//! One worker applies access changes to LiveKit, one at a time and in order, so a decision
//! made before a later change never lands after it. Requests coalesce instead of piling up:
//! a burst of joins in one room is one pending room check, and a mute change goes first.

use std::{
    collections::BTreeSet,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use rusttalk_core::auth::voice_identity;
use serde_json::json;
use sqlx::query_scalar;
use tokio::{sync::Notify, time::Instant};

use super::{Action, DATA_PREFIX, LiveKit, decide, room_channel};
use crate::{ApiResult, App};

/// How long a failed check waits before it is tried again; other work goes on meanwhile.
const RETRY: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct Queue {
    pending: Mutex<Pending>,
    wake: Notify,
}

#[derive(Default)]
struct Pending {
    /// People whose voice mute changed, checked first.
    persons: BTreeSet<Vec<u8>>,
    /// Every room, as after a membership or role change.
    full: bool,
    /// Voice channels whose rooms to check, as after a join.
    rooms: BTreeSet<String>,
    /// Checks that failed, each with when to try it again.
    retry: Vec<(Instant, Job)>,
}

#[derive(Debug, PartialEq)]
enum Job {
    Persons(BTreeSet<Vec<u8>>),
    Full,
    Room(String),
}

impl Pending {
    /// The next check ready at `now`; failed checks rejoin the queue once their time comes.
    fn next(&mut self, now: Instant) -> Option<Job> {
        let (due, waiting) = std::mem::take(&mut self.retry)
            .into_iter()
            .partition(|(at, _)| *at <= now);
        self.retry = waiting;
        for (_, job) in due {
            self.put_back(job);
        }
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

    fn earliest_retry(&self) -> Option<Instant> {
        self.retry.iter().map(|(at, _)| *at).min()
    }
}

impl Queue {
    fn add(&self, add: impl FnOnce(&mut Pending)) {
        add(&mut self.pending.lock().expect("pending voice checks"));
        self.wake.notify_one();
    }

    /// Run checks forever, one at a time. `run` hands back a check that failed.
    async fn work<F: Future<Output = Option<Job>>>(&self, mut run: impl FnMut(Job) -> F) {
        loop {
            let (job, retry_at) = {
                let mut pending = self.pending.lock().expect("pending voice checks");
                (pending.next(Instant::now()), pending.earliest_retry())
            };
            let Some(job) = job else {
                match retry_at {
                    Some(at) => {
                        tokio::select! {
                            () = self.wake.notified() => {}
                            () = tokio::time::sleep_until(at) => {}
                        }
                    }
                    None => self.wake.notified().await,
                }
                continue;
            };
            if let Some(failed) = run(job).await {
                // LiveKit can be briefly unreachable, and lost access must still be taken away.
                let mut pending = self.pending.lock().expect("pending voice checks");
                pending.retry.push((Instant::now() + RETRY, failed));
            }
        }
    }
}

/// Run what `worker` makes again, after `restarted`, whenever it panics. Dropping this future
/// (server shutdown) stops the running worker too.
async fn supervise<F>(mut worker: impl FnMut() -> F, restarted: impl Fn())
where
    F: Future<Output = ()> + Send + 'static,
{
    struct Abort(tokio::task::JoinHandle<()>);
    impl Drop for Abort {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    loop {
        let mut task = Abort(tokio::spawn(worker()));
        match (&mut task.0).await {
            Err(error) if error.is_panic() => {
                tracing::error!("voice access worker stopped unexpectedly; restarting it");
                tokio::time::sleep(Duration::from_secs(1)).await;
                restarted();
            }
            _ => return,
        }
    }
}

fn queue(app: &App, add: impl FnOnce(&mut Pending)) {
    if let Some(livekit) = app.voice.as_deref() {
        livekit.queue.add(add);
    }
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

/// Start the worker under a supervisor. Each start first applies everything, which also
/// covers what a restart, a crash or a LiveKit outage left unapplied.
pub fn start(app: &App) {
    let Some(livekit) = app.voice.clone() else {
        return;
    };
    let full = |livekit: &Arc<LiveKit>| livekit.queue.add(|pending| pending.full = true);
    full(&livekit);
    let app = app.clone();
    let restarted = {
        let livekit = livekit.clone();
        move || full(&livekit)
    };
    tokio::spawn(supervise(
        move || {
            let (app, livekit) = (app.clone(), livekit.clone());
            async move {
                livekit
                    .queue
                    .work(|job| {
                        let (app, livekit) = (&app, &livekit);
                        async move {
                            let complete = run(app, livekit, &job).await.unwrap_or_else(|error| {
                                tracing::warn!(reason = error.code, "could not check voice access");
                                false
                            });
                            (!complete).then_some(job)
                        }
                    })
                    .await;
            }
        },
        restarted,
    ));
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
        let now = Instant::now();
        let mut pending = Pending::default();
        // A burst of joins in room B while a role change is waiting.
        for _ in 0..10_000 {
            pending.rooms.insert("B".into());
        }
        pending.full = true;
        // Then a mute in room A.
        pending.persons.insert(vec![1]);
        assert_eq!(pending.next(now), Some(Job::Persons([vec![1]].into())));
        assert_eq!(pending.next(now), Some(Job::Full));
        assert_eq!(pending.next(now), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_check_waits_while_other_work_goes_on() {
        let queue = Arc::new(Queue::default());
        let (done, mut ran) = tokio::sync::mpsc::unbounded_channel();
        let worker = {
            let queue = queue.clone();
            tokio::spawn(async move {
                queue
                    .work(|job| {
                        let done = done.clone();
                        async move {
                            // Room B's LiveKit calls keep failing.
                            let failed = job == Job::Room("B".into());
                            done.send(job).unwrap();
                            failed.then_some(Job::Room("B".into()))
                        }
                    })
                    .await
            })
        };
        queue.add(|pending| pending.rooms.extend(["B".into(), "C".into()]));
        assert_eq!(ran.recv().await, Some(Job::Room("B".into())));
        assert_eq!(ran.recv().await, Some(Job::Room("C".into())));
        // A mute queued while B waits for its retry runs straight away.
        queue.add(|pending| {
            pending.persons.insert(vec![7]);
        });
        let started = Instant::now();
        assert_eq!(ran.recv().await, Some(Job::Persons([vec![7]].into())));
        assert!(started.elapsed() < RETRY);
        // B is tried again once its own time comes.
        assert_eq!(ran.recv().await, Some(Job::Room("B".into())));
        assert!(started.elapsed() >= RETRY - Duration::from_millis(1));
        worker.abort();
    }

    #[tokio::test]
    async fn a_panicking_worker_is_restarted_with_a_full_check() {
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let restarts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let supervisor = tokio::spawn({
            let (starts, restarts) = (starts.clone(), restarts.clone());
            supervise(
                move || {
                    let first = starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                    async move {
                        if first {
                            panic!("forced");
                        }
                        std::future::pending::<()>().await;
                    }
                },
                move || {
                    restarts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                },
            )
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while starts.load(std::sync::atomic::Ordering::SeqCst) < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the worker was not restarted");
        assert_eq!(restarts.load(std::sync::atomic::Ordering::SeqCst), 1);
        supervisor.abort();
    }
}
