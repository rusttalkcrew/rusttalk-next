//! Which audio to receive, and who may receive the sound of our screen share.
//!
//! A screen share's sound is meant for the people watching it. Receivers only ask for
//! it while they watch, but media uses the channel key, so an older client that takes
//! every audio track could still listen. The publisher therefore also allows each track
//! only to the members who announced (see `watch`) that they watch the share.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    time::Duration,
};

use anyhow::Result;
use livekit::{
    Room,
    e2ee::EncryptionType,
    participant::ParticipantTrackPermission,
    prelude::{RemoteTrackPublication, TrackKind},
};
use tokio::{
    sync::{Notify, mpsc::UnboundedReceiver},
    task::JoinHandle,
};

use super::{ParticipantIdentity, RoomEvent, TrackSid, TrackSource, VoiceRoom, track_name, watch};
use crate::keys::Key;

/// Who may receive which of our tracks. `None` lets everyone have everything; otherwise
/// each member gets all tracks (`None`) or only the listed ones.
pub(super) type Permissions = Option<Vec<(ParticipantIdentity, Option<Vec<TrackSid>>)>>;

/// Whether to receive an audio track: voices always, shared apps once their name
/// opens as app audio, and a share's sound only while watching the share. A label we
/// cannot open yet (a newer epoch's key is still on its way) stays off until it opens.
fn wanted(
    source: TrackSource,
    name: &str,
    app_names: &HashMap<i64, Key>,
    stream_names: &HashMap<i64, Key>,
    screen_watched: bool,
) -> bool {
    if source != TrackSource::ScreenshareAudio {
        return true;
    }
    if track_name::decrypt(app_names, track_name::APP_AUDIO, name).is_some() {
        return true;
    }
    screen_watched && track_name::decrypt(stream_names, track_name::STREAM_AUDIO, name).is_some()
}

pub(super) fn is_stream_audio(
    publication: &RemoteTrackPublication,
    stream_names: &HashMap<i64, Key>,
) -> bool {
    publication.source() == TrackSource::ScreenshareAudio
        && track_name::decrypt(stream_names, track_name::STREAM_AUDIO, &publication.name())
            .is_some()
}

/// Subscribe a publication when `wanted`; never unsubscribes (see `set_subscribed`).
pub(super) fn subscribe_audio(
    publication: &RemoteTrackPublication,
    app_names: &HashMap<i64, Key>,
    stream_names: &HashMap<i64, Key>,
    screen_watched: bool,
) {
    if publication.kind() == TrackKind::Audio
        && publication.encryption_type() == EncryptionType::Gcm
        && !publication.is_desired()
        && wanted(
            publication.source(),
            &publication.name(),
            app_names,
            stream_names,
            screen_watched,
        )
    {
        publication.set_subscribed(true);
    }
}

/// While any of our tracks is stream sound (or `restrict` is set, just before
/// publishing one), members who don't watch our share get every other track by ID.
fn permissions(
    restrict: bool,
    local: &[(TrackSid, bool)],
    members: &[(ParticipantIdentity, bool)],
) -> Permissions {
    if !restrict && !local.iter().any(|(_, sound)| *sound) {
        return None;
    }
    let mut others: Vec<_> = (local.iter())
        .filter(|(_, sound)| !sound)
        .map(|(sid, _)| sid.clone())
        .collect();
    others.sort();
    let mut list: Vec<_> = (members.iter())
        .map(|(id, watching)| (id.clone(), (!watching).then(|| others.clone())))
        .collect();
    list.sort_by(|a, b| a.0.cmp(&b.0));
    Some(list)
}

impl VoiceRoom {
    /// Ask for every audio track we want now: after a key arrives, a name may open.
    pub(super) fn reconcile_audio(&self) {
        for participant in self.room.remote_participants().into_values() {
            let publications = participant.track_publications();
            if self.blocked.contains(participant.identity().as_str()) {
                for publication in publications
                    .values()
                    .filter(|p| p.is_desired() || p.is_subscribed())
                {
                    publication.set_subscribed(false);
                }
                continue;
            }
            let watched = (publications.values())
                .any(|p| p.source() == TrackSource::Screenshare && p.is_desired());
            for publication in publications.values() {
                subscribe_audio(publication, &self.name_keys, &self.stream_names, watched);
            }
        }
    }
}

/// How long the permissions may stay wrong after a lost update, while restricted.
const REFRESH: Duration = Duration::from_secs(2);

/// What should be allowed now, and a way to send it.
pub(super) trait Grants: Send + Sync + 'static {
    fn desired(&self) -> Permissions;
    fn send(&self, permissions: Permissions) -> impl Future<Output = Result<()>> + Send;
}

/// Keeps LiveKit's track permissions at `Grants::desired`. It runs in its own task:
/// an update cancelled halfway (the caller's future dropped) would otherwise be
/// forgotten, leaving a newcomer without any of our tracks.
pub(super) struct Reconciler<G> {
    pub(super) grants: G,
    /// The last permissions LiveKit took; only completed sends land here. The lock
    /// also keeps updates in order.
    sent: tokio::sync::Mutex<Option<Permissions>>,
    wake: Notify,
    refresh: Duration,
}

impl<G: Grants> Reconciler<G> {
    pub(super) fn new(grants: G) -> Self {
        Self::with_refresh(grants, REFRESH)
    }

    fn with_refresh(grants: G, refresh: Duration) -> Self {
        Self {
            grants,
            sent: tokio::sync::Mutex::new(None),
            wake: Notify::new(),
            refresh,
        }
    }

    /// Check again soon, for example after keys changed: a viewer's announcement may
    /// open now.
    pub(super) fn wake(&self) {
        self.wake.notify_one();
    }

    /// Send what should be allowed unless LiveKit already has it; `force` sends anyway.
    pub(super) async fn apply(&self, force: bool) -> Result<()> {
        let mut sent = self.sent.lock().await;
        let wanted = self.grants.desired();
        if !force && sent.as_ref() == Some(&wanted) {
            return Ok(());
        }
        // The SDK stores the new permissions before it signals them: if this is cancelled
        // or fails halfway, nobody knows what LiveKit has, so the next check must send.
        *sent = None;
        self.grants.send(wanted.clone()).await?;
        *sent = Some(wanted);
        Ok(())
    }

    /// Follow room changes, wake-ups and, while restricted, a regular refresh that
    /// repairs a lost or failed update. Ends with the room's events.
    pub(super) async fn run(self: Arc<Self>, mut events: UnboundedReceiver<RoomEvent>) {
        // One clock for the whole run: busy rooms (speakers change all the time) must
        // not keep pushing the repair back.
        let first = tokio::time::Instant::now() + self.refresh;
        let mut refresh = tokio::time::interval_at(first, self.refresh);
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let refresh = tokio::select! {
                event = events.recv() => match event {
                    None => return,
                    Some(event) if changes_grants(&event) => false,
                    Some(_) => continue,
                },
                () = self.wake.notified() => false,
                _ = refresh.tick() => true,
            };
            let force = refresh && self.grants.desired().is_some();
            if let Err(error) = self.apply(force).await {
                tracing::warn!(%error, "updating who may receive our tracks failed");
            }
        }
    }
}

/// Who is here, who watches our share, or which tracks we publish changed.
fn changes_grants(event: &RoomEvent) -> bool {
    matches!(
        event,
        RoomEvent::ParticipantConnected(_)
            | RoomEvent::ParticipantDisconnected(_)
            | RoomEvent::ParticipantAttributesChanged { .. }
            | RoomEvent::LocalTrackPublished { .. }
            | RoomEvent::LocalTrackUnpublished { .. }
            | RoomEvent::LocalTrackRepublished { .. }
            | RoomEvent::Reconnected
    )
}

/// The keys `RoomGrants` needs, refreshed by `VoiceRoom::sync_key`.
#[derive(Default)]
pub(super) struct GrantKeys {
    pub(super) stream_names: HashMap<i64, Key>,
    pub(super) watch_keys: HashMap<i64, Key>,
}

/// Whether our stream sound is published or on its way, which restricts the others.
#[derive(Default)]
pub(super) struct Sound {
    /// Publications in progress: restrict before the track exists.
    pub(super) publishing: AtomicUsize,
    /// Published and not stopped by us. A bitrate change or a reconnect republishes
    /// the track, and it must not reach non-viewers while it briefly has none.
    pub(super) wanted: AtomicBool,
}

impl Sound {
    fn restricts(&self) -> bool {
        self.publishing.load(Relaxed) > 0 || self.wanted.load(Relaxed)
    }
}

/// The real room's permissions: see the module comment.
pub(super) struct RoomGrants {
    pub(super) room: Arc<Room>,
    pub(super) keys: std::sync::Mutex<GrantKeys>,
    pub(super) sound: Sound,
}

impl Grants for RoomGrants {
    fn desired(&self) -> Permissions {
        let keys = self.keys.lock().expect("grant keys");
        let local_participant = self.room.local_participant();
        let publications = local_participant.track_publications();
        let screen = (publications.values())
            .find(|p| p.source() == TrackSource::Screenshare)
            .map(|p| p.sid());
        let local: Vec<_> = (publications.values())
            .map(|p| {
                let sound = p.source() == TrackSource::ScreenshareAudio
                    && track_name::decrypt(&keys.stream_names, track_name::STREAM_AUDIO, &p.name())
                        .is_some();
                (p.sid(), sound)
            })
            .collect();
        let members: Vec<_> = (self.room.remote_participants().into_values())
            .map(|member| {
                let watching = screen.as_ref().is_some_and(|screen| {
                    watch::watches(&keys.watch_keys, &member).contains(screen)
                });
                (member.identity(), watching)
            })
            .collect();
        permissions(self.sound.restricts(), &local, &members)
    }

    async fn send(&self, permissions: Permissions) -> Result<()> {
        let list = (permissions.iter().flatten())
            .map(|(id, allowed)| ParticipantTrackPermission {
                participant_identity: id.clone(),
                allow_all: allowed.is_none(),
                allowed_track_sids: allowed.clone().unwrap_or_default(),
            })
            .collect();
        (self.room.local_participant())
            .set_track_subscription_permissions(permissions.is_none(), list)
            .await?;
        Ok(())
    }
}

/// Counts a stream sound publication in progress until dropped, also when cancelled.
pub(super) struct Publishing<'a>(pub(super) &'a AtomicUsize);

impl<'a> Publishing<'a> {
    pub(super) fn start(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Relaxed);
        Self(count)
    }
}

impl Drop for Publishing<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

/// Aborts the reconciler's task with the room.
pub(super) struct AbortOnDrop(pub(super) JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroizing;

    use super::*;

    fn sid(s: &str) -> TrackSid {
        TrackSid::try_from(format!("TR_{s}")).unwrap()
    }

    fn id(s: &str) -> ParticipantIdentity {
        ParticipantIdentity::from(s.to_owned())
    }

    #[test]
    fn audio_is_received_only_once_its_label_opens() {
        let old = HashMap::from([(3, Zeroizing::new([1; 32]))]);
        let new = Zeroizing::new([2; 32]);
        let both = HashMap::from([(3, old[&3].clone()), (4, new.clone())]);
        let app = track_name::encrypt(&new, track_name::APP_AUDIO, 4, "Spotify").unwrap();
        let sound = track_name::encrypt(&new, track_name::STREAM_AUDIO, 4, "Game").unwrap();
        let mic = TrackSource::Microphone;
        let shared = TrackSource::ScreenshareAudio;
        assert!(wanted(mic, "microphone", &old, &old, false));
        // Published in epoch 4 before its key arrived: neither app audio nor sound yet.
        assert!(!wanted(shared, &app, &old, &old, false));
        assert!(!wanted(shared, &sound, &old, &old, true));
        // With the key, the app plays; the share's sound only while watching.
        assert!(wanted(shared, &app, &both, &both, false));
        assert!(!wanted(shared, &sound, &both, &both, false));
        assert!(wanted(shared, &sound, &both, &both, true));
        assert!(!wanted(shared, "plain name", &both, &both, true));
    }

    #[test]
    fn only_viewers_may_receive_the_share_sound() {
        let local = [(sid("mic"), false), (sid("screen"), false)];
        let members = [(id("b"), true), (id("a"), false)];
        // No sound published: nothing is restricted.
        assert_eq!(permissions(false, &local, &members), None);
        let others = Some(vec![sid("mic"), sid("screen")]);
        let expected = Some(vec![(id("a"), others.clone()), (id("b"), None)]);
        // About to publish sound: non-viewers already get only the other tracks.
        assert_eq!(permissions(true, &local, &members), expected);
        let with_sound = [local[0].clone(), local[1].clone(), (sid("sound"), true)];
        assert_eq!(permissions(false, &with_sound, &members), expected);
        assert_eq!(
            permissions(false, &with_sound, &[]),
            Some(Vec::new()),
            "alone: nobody may subscribe to anything, and newcomers are re-checked"
        );
    }

    /// Permissions from a shared value; sends can be held at a gate or fail.
    struct Fake {
        desired: std::sync::Mutex<Permissions>,
        gate: tokio::sync::Semaphore,
        fail: std::sync::atomic::AtomicBool,
        /// What the SDK holds: set before signalling, like `LocalParticipant` does.
        stored: std::sync::Mutex<Permissions>,
        tries: AtomicUsize,
        sent: std::sync::Mutex<Vec<Permissions>>,
    }

    impl Fake {
        fn new(desired: Permissions) -> Self {
            Self {
                desired: std::sync::Mutex::new(desired),
                gate: tokio::sync::Semaphore::new(0),
                fail: Default::default(),
                stored: Default::default(),
                tries: Default::default(),
                sent: Default::default(),
            }
        }
    }

    impl Grants for Fake {
        fn desired(&self) -> Permissions {
            self.desired.lock().unwrap().clone()
        }

        async fn send(&self, permissions: Permissions) -> Result<()> {
            *self.stored.lock().unwrap() = permissions.clone();
            // Signalling blocked: nothing is enqueued until the gate opens.
            self.gate.acquire().await?.forget();
            self.tries.fetch_add(1, Relaxed);
            anyhow::ensure!(!self.fail.load(Relaxed), "signalling failed");
            self.sent.lock().unwrap().push(permissions);
            Ok(())
        }
    }

    fn restricted(watching: bool) -> Permissions {
        Some(vec![(id("viewer"), (!watching).then(|| vec![sid("mic")]))])
    }

    async fn until(what: impl Fn() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !what() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition within 5 s");
    }

    #[tokio::test]
    async fn a_cancelled_update_is_sent_again() {
        let reconciler = Arc::new(Reconciler::with_refresh(
            Fake::new(restricted(false)),
            Duration::from_secs(3600),
        ));
        // A caller gives up while signalling is blocked: nothing may count as sent.
        let cancelled =
            tokio::time::timeout(Duration::from_millis(20), reconciler.apply(false)).await;
        assert!(cancelled.is_err());
        assert!(reconciler.sent.lock().await.is_none());
        // The reconciler's own task still delivers it once signalling works.
        let (_events, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(reconciler.clone().run(receiver));
        reconciler.grants.gate.add_permits(10);
        reconciler.wake();
        until(|| reconciler.grants.sent.lock().unwrap().len() == 1).await;
        assert_eq!(reconciler.grants.sent.lock().unwrap()[0], restricted(false));
    }

    #[tokio::test]
    async fn a_failed_update_is_repaired_while_restricted() {
        let fake = Fake::new(restricted(false));
        fake.fail.store(true, Relaxed);
        fake.gate.add_permits(100);
        let reconciler = Arc::new(Reconciler::with_refresh(fake, Duration::from_millis(20)));
        let (_events, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(reconciler.clone().run(receiver));
        reconciler.wake();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(reconciler.grants.sent.lock().unwrap().is_empty());
        // No event, no wake-up: the refresh alone gets it through.
        reconciler.grants.fail.store(false, Relaxed);
        until(|| !reconciler.grants.sent.lock().unwrap().is_empty()).await;
    }

    #[test]
    fn a_republished_sound_stays_away_from_non_viewers() {
        let sound = Sound::default();
        let members = [(id("viewer"), true), (id("other"), false)];
        let mic = (sid("mic"), false);
        {
            let _publishing = Publishing::start(&sound.publishing);
            assert!(sound.restricts(), "restricted before the track exists");
        }
        sound.wanted.store(true, Relaxed);
        let before = permissions(
            sound.restricts(),
            &[mic.clone(), (sid("sound"), true)],
            &members,
        );
        // A bitrate change: the old track is gone, the new one not yet listed.
        let between = permissions(sound.restricts(), std::slice::from_ref(&mic), &members);
        assert_eq!(between, before);
        let other = &between.as_ref().unwrap()[0];
        assert_eq!(other, &(id("other"), Some(vec![sid("mic")])));
        // Republished under a new ID, which the non-viewer's list does not hold.
        let after = permissions(
            sound.restricts(),
            &[mic.clone(), (sid("new"), true)],
            &members,
        );
        assert_eq!(after, before);
        // Only our own stop lifts it.
        sound.wanted.store(false, Relaxed);
        assert_eq!(permissions(sound.restricts(), &[mic], &members), None);
    }

    #[tokio::test]
    async fn a_cancelled_restriction_is_undone() {
        let reconciler = Reconciler::with_refresh(Fake::new(None), Duration::from_secs(3600));
        reconciler.grants.gate.add_permits(1);
        reconciler.apply(false).await.unwrap();
        assert_eq!(*reconciler.sent.lock().await, Some(None));
        // A stream sound publish starts restricting, and is cancelled while signalling
        // is blocked, after the SDK already stored the restriction.
        *reconciler.grants.desired.lock().unwrap() = restricted(false);
        let cancelled =
            tokio::time::timeout(Duration::from_millis(20), reconciler.apply(false)).await;
        assert!(cancelled.is_err());
        assert_eq!(*reconciler.grants.stored.lock().unwrap(), restricted(false));
        // Back to no sound: the open permissions must be sent again, not skipped.
        *reconciler.grants.desired.lock().unwrap() = None;
        reconciler.grants.gate.add_permits(1);
        reconciler.apply(false).await.unwrap();
        assert_eq!(*reconciler.grants.stored.lock().unwrap(), None);
        assert_eq!(*reconciler.grants.sent.lock().unwrap(), [None, None]);
    }

    #[tokio::test(start_paused = true)]
    async fn repair_happens_while_other_events_keep_coming() {
        let fake = Fake::new(restricted(false));
        fake.fail.store(true, Relaxed);
        fake.gate.add_permits(10_000);
        let reconciler = Arc::new(Reconciler::with_refresh(fake, Duration::from_millis(50)));
        let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(reconciler.clone().run(receiver));
        reconciler.wake();
        while reconciler.grants.tries.load(Relaxed) == 0 {
            tokio::task::yield_now().await;
        }
        assert!(
            reconciler.grants.sent.lock().unwrap().is_empty(),
            "the first try failed"
        );
        reconciler.grants.fail.store(false, Relaxed);
        // Speakers change every 10 ms, faster than the 50 ms refresh, for 200 ms.
        for step in 1..=20 {
            let busy = RoomEvent::ActiveSpeakersChanged {
                speakers: Vec::new(),
            };
            events.send(busy).unwrap();
            tokio::time::advance(Duration::from_millis(10)).await;
            tokio::task::yield_now().await;
            if step == 5 {
                // The first refresh, at 50 ms, repaired it despite the events.
                assert_eq!(*reconciler.grants.sent.lock().unwrap(), [restricted(false)]);
            }
        }
        // While restricted, one resend per refresh: at 50, 100, 150 and 200 ms.
        assert_eq!(reconciler.grants.tries.load(Relaxed), 5, "no bursts");
        assert_eq!(reconciler.grants.sent.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn a_watch_announced_before_its_key_counts_once_the_key_arrives() {
        let fake = Fake::new(restricted(false));
        fake.gate.add_permits(100);
        let reconciler = Arc::new(Reconciler::with_refresh(fake, Duration::from_secs(3600)));
        let (_events, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(reconciler.clone().run(receiver));
        reconciler.wake();
        until(|| reconciler.grants.sent.lock().unwrap().len() == 1).await;
        // The viewer's announcement opens with the new key; `sync_key` wakes us.
        *reconciler.grants.desired.lock().unwrap() = restricted(true);
        reconciler.wake();
        until(|| reconciler.grants.sent.lock().unwrap().len() == 2).await;
        assert_eq!(reconciler.grants.sent.lock().unwrap()[1], restricted(true));
    }
}
