//! Device sync (PROMPT 6.4): this person's joined servers and synced settings, merged
//! with the encrypted copy kept on every joined server. Never log settings or addresses.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result};
use rusttalk_core::{
    auth::{KnownServer, Session},
    client::{
        Server as Api,
        revocations::Revocations,
        save_json,
        sync::{self, Failures, Snapshot, State, VersionConflict},
    },
    identity::fingerprint,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::{
    audio::Settings,
    dm::{SYNC_BUDGET, Tombstones},
};

/// Voice and video preferences, dismissed official servers, the appearance and language,
/// and the profile name and bio, that follow the person to their other devices.
/// The microphone, speakers and camera differ per computer and stay in `voice.json` only.
const SYNCED: [&str; 19] = [
    "push_to_talk",
    "key",
    "threshold",
    "auto_threshold",
    "echo_cancellation",
    "noise_suppression",
    "auto_gain",
    "camera_height",
    "camera_fps",
    "dismissed_official",
    "theme_mode",
    "dark_theme",
    "light_theme",
    "accent",
    "language",
    "friends_only",
    "display_name",
    "bio",
    "name_overrides",
];

pub struct DeviceSync {
    saved: tokio::sync::Mutex<Saved>,
    /// Connected servers with their current sessions.
    peers: Mutex<HashMap<Url, (Api, Session)>>,
    /// Peers loaded by a finished run since their session last changed.
    fresh: Mutex<HashSet<Url>>,
    /// Coalescing: callers take a ticket; a run that finishes covers every ticket
    /// taken before it started, so queued callers return at once.
    requested: AtomicU64,
    finished: AtomicU64,
}

/// `sync.json`: the last merged snapshot, so one reachable server can't roll this profile back.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    snapshot: Snapshot,
    /// Local edits no server has accepted yet, applied again after each merge.
    pending: Vec<Edit>,
    #[serde(default)]
    avatar: Option<([u8; 32], String)>,
    /// Servers whose pin conflict was already shown.
    #[serde(skip)]
    warned: HashSet<Url>,
}

/// The synced setting with this person's deleted DM messages (`dm::Tombstones`), so
/// copies on servers that were away get deleted there too and are never shown.
pub const DM_DELETED: &str = "dm_deleted";

#[derive(Serialize, Deserialize)]
pub enum Edit {
    /// Bootstrap old profiles without replacing a default learned from another device.
    InitialAvatar(String),
    Join(KnownServer),
    Leave(Url),
    Setting(String, Value),
    /// A friend, by hex person ID (see `friends`).
    Contact(String, Value),
    /// One of this person's DM messages, deleted, and copies (`dm::copy`) that may remain.
    /// Added to the other deletes rather than replacing them.
    DeletedDm([u8; 32], Vec<String>),
    /// That copy (`dm::copy`) of a deleted DM message is gone.
    DmCopyGone([u8; 32], String),
}

impl Edit {
    pub(crate) fn apply(&self, state: &mut State) {
        match self {
            Self::InitialAvatar(digest) => {
                state
                    .settings
                    .entry("avatar_digest".into())
                    .or_insert_with(|| digest.clone().into());
            }
            // Re-applying an unchanged join must not move the server to the end of the list.
            Self::Join(server) if state.servers.contains(server) => {}
            Self::Join(server) => state.join(server.clone()),
            Self::Leave(address) => state.leave(address),
            Self::Setting(key, value) => {
                state.settings.insert(key.clone(), value.clone());
            }
            // Re-applied after each merge: a later change from another device stays.
            Self::Contact(key, value) => {
                let later = (state.contacts.get(key))
                    .is_some_and(|current| sync::changed(current) > sync::changed(value));
                if !later {
                    state.contacts.insert(key.clone(), value.clone());
                }
            }
            Self::DeletedDm(id, servers) => {
                let mut deletes = dm_deleted(&state.settings);
                deletes.add(*id, servers.iter().cloned());
                set_dm_deleted(state, &deletes);
            }
            Self::DmCopyGone(id, server) => {
                let mut deletes = dm_deleted(&state.settings);
                deletes.gone(id, server);
                set_dm_deleted(state, &deletes);
            }
        }
    }
}

/// This person's deleted DM messages in the synced settings.
pub fn dm_deleted(settings: &BTreeMap<String, Value>) -> Tombstones {
    (settings.get(DM_DELETED).cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn set_dm_deleted(state: &mut State, deletes: &Tombstones) {
    let value = serde_json::to_value(deletes.bounded(SYNC_BUDGET)).expect("deletes serialize");
    state.settings.insert(DM_DELETED.into(), value);
}

/// Keep every delete seen in any copy: the settings merge takes one snapshot's field
/// whole, and a device that already published has no pending edit to apply again.
fn keep_deletes(state: &mut State, seen: &Tombstones) {
    let mut deletes = dm_deleted(&state.settings);
    deletes.merge(seen);
    set_dm_deleted(state, &deletes);
}

/// The merged state for the UI.
pub struct Synced {
    pub servers: Vec<KnownServer>,
    pub settings: BTreeMap<String, Value>,
    pub contacts: BTreeMap<String, Value>,
    /// Servers another device pinned differently. They are never adopted.
    pub conflicts: Vec<Url>,
}

impl DeviceSync {
    pub fn open(data: &Path) -> Result<Self> {
        let saved = crate::load_json(&data.join("sync.json")).context("load sync state")?;
        Ok(Self {
            saved: tokio::sync::Mutex::new(saved),
            peers: Mutex::default(),
            fresh: Mutex::default(),
            requested: AtomicU64::new(0),
            finished: AtomicU64::new(0),
        })
    }

    /// Joined servers, to reconnect at start. Not for async code.
    pub fn servers(&self) -> Vec<KnownServer> {
        self.saved.blocking_lock().snapshot.state.servers.clone()
    }

    /// Friends and other contacts as last merged, for the start. Not for async code.
    pub fn contacts(&self) -> BTreeMap<String, Value> {
        self.saved.blocking_lock().snapshot.state.contacts.clone()
    }

    /// Deleted DM messages as last saved, at start. Not for async code.
    pub fn dm_deleted(&self) -> Tombstones {
        dm_deleted(&self.saved.blocking_lock().snapshot.state.settings)
    }

    pub async fn current_dm_deletes(&self) -> Tombstones {
        dm_deleted(&self.saved.lock().await.snapshot.state.settings)
    }

    /// Whether local edits are still waiting to be published to peers.
    pub async fn has_pending(&self) -> bool {
        !self.saved.lock().await.pending.is_empty()
    }

    /// Cache only this account's selected image. A digest in encrypted device sync
    /// avoids putting a 256 KiB image into the 256 KiB settings blob.
    pub async fn avatar(
        &self,
        data: &Path,
        person: &[u8; 32],
        candidate: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>> {
        use rusttalk_core::auth::{decode_bytes, encode};
        use sha2::{Digest, Sha256};
        let digest = |bytes: &[u8]| encode(&Sha256::digest(bytes));
        let mut saved = self.saved.lock().await;
        if let Some(bytes) = candidate {
            let hash = digest(bytes);
            let mut dirty = false;
            if !saved.snapshot.state.settings.contains_key("avatar_digest") {
                let edit = Edit::InitialAvatar(hash.clone());
                edit.apply(&mut saved.snapshot.state);
                saved.pending.push(edit);
                dirty = true;
            }
            if saved
                .snapshot
                .state
                .settings
                .get("avatar_digest")
                .and_then(Value::as_str)
                == Some(&hash)
            {
                let encoded = encode(bytes);
                let cached = (*person, encoded);
                if saved.avatar.as_ref() != Some(&cached) {
                    saved.avatar = Some(cached);
                    dirty = true;
                }
            }
            if dirty {
                save(data, &saved)?;
            }
        }
        let bytes = saved
            .avatar
            .as_ref()
            .filter(|(owner, _)| owner == person)
            .and_then(|(_, bytes)| match decode_bytes(bytes) {
                Ok(bytes) => Some(bytes),
                Err(error) => {
                    tracing::warn!(%error, "ignoring unreadable cached account picture");
                    None
                }
            });
        Ok(bytes.filter(|bytes| {
            saved
                .snapshot
                .state
                .settings
                .get("avatar_digest")
                .and_then(Value::as_str)
                == Some(digest(bytes).as_str())
        }))
    }

    pub async fn select_avatar(&self, data: &Path, person: &[u8; 32], bytes: &[u8]) -> Result<()> {
        use rusttalk_core::auth::encode;
        use sha2::{Digest, Sha256};
        rusttalk_core::client::decode_profile_image(bytes)?;
        let mut saved = self.saved.lock().await;
        let edit = Edit::Setting(
            "avatar_digest".into(),
            encode(&Sha256::digest(bytes)).into(),
        );
        edit.apply(&mut saved.snapshot.state);
        saved.pending.push(edit);
        saved.avatar = Some((*person, encode(bytes)));
        save(data, &saved)
    }

    /// A new or changed session (a reconnect) needs a run before `is_current` again.
    pub fn set_peer(&self, address: &Url, peer: Option<(Api, Session)>) {
        let mut peers = lock(&self.peers);
        let same = match &peer {
            Some((_, session)) => peers
                .get(address)
                .is_some_and(|(_, old)| old.token == session.token),
            None => false,
        };
        if !same {
            lock(&self.fresh).remove(address);
        }
        match peer {
            Some(peer) => peers.insert(address.clone(), peer),
            None => peers.remove(address),
        };
    }

    /// Whether a run already loaded from this session and no local edit waits:
    /// another run would change nothing.
    /// Never waits: a run in progress (holding the state) counts as not current.
    pub fn is_current(&self, address: &Url) -> bool {
        lock(&self.fresh).contains(address)
            && (self.saved.try_lock()).is_ok_and(|saved| saved.pending.is_empty())
    }

    /// Record local edits; the next `run` publishes them.
    pub async fn edit(&self, data: &Path, edits: Vec<Edit>) -> Result<()> {
        let mut saved = self.saved.lock().await;
        for edit in edits {
            edit.apply(&mut saved.snapshot.state);
            saved.pending.push(edit);
        }
        save(data, &saved)
    }

    /// Apply local edits, load from every connected server, merge, and publish the
    /// result to all of them. `show` gets the result while no other sync can run.
    pub async fn run(
        &self,
        data: &Path,
        identity: &rusttalk_core::identity::DeviceIdentity,
        edits: Vec<Edit>,
        show: impl FnOnce(Synced),
    ) -> Result<()> {
        if !edits.is_empty() {
            self.edit(data, edits).await?;
        }
        // Taken after the edits and peers are recorded, so a run starting later includes them.
        let ticket = self.requested.fetch_add(1, Ordering::SeqCst) + 1;
        let mut saved = self.saved.lock().await;
        if self.finished.load(Ordering::SeqCst) >= ticket {
            // A run that started after this request finished and already showed its result.
            return Ok(());
        }
        let covers = self.requested.load(Ordering::SeqCst);
        let connected: Vec<(Url, Api, Session)> = lock(&self.peers)
            .iter()
            .map(|(address, (api, session))| (address.clone(), api.clone(), session.clone()))
            .collect();
        let peers: Vec<sync::Peer> = (connected.iter())
            .map(|(_, api, session)| (api, session))
            .collect();
        let mut failed = Vec::new();
        let key = &identity.sync_key;
        let person = identity.certificate.person_id();
        let records = Revocations::in_profile(data);
        let mut conflicts = Vec::new();
        // A version conflict means another device published in between: load and merge again.
        for _ in 0..3 {
            let (snapshots, load_failures) = sync::load_all(&peers, key).await;
            log("load", &load_failures);
            failed.extend(unreached(&connected, &load_failures));
            let loaded = snapshots.len();
            let peers_agree = snapshots.first().is_some_and(|first| {
                snapshots
                    .iter()
                    .skip(1)
                    .all(|snap| snap.version == first.version && snap.state == first.state)
            });
            let first_meta = snapshots
                .first()
                .map(|first| (first.version, first.state.clone()));
            let mut deletes = dm_deleted(&saved.snapshot.state.settings);
            let (verified, relay_failures) = records
                .relay(
                    person,
                    std::iter::once(&saved.snapshot).chain(&snapshots),
                    &peers,
                )
                .await?;
            failed.extend(unreached(&connected, &relay_failures));
            for snapshot in &snapshots {
                deletes.merge(&dm_deleted(&snapshot.state.settings));
            }
            if let Some(remote) = snapshots.into_iter().next() {
                for conflict in sync::merge(data, &mut saved.snapshot, remote)? {
                    tracing::warn!(reason = %conflict.reason, "a synced server pin differs; not adopted");
                    if saved.warned.insert(conflict.address.clone()) {
                        conflicts.push(conflict.address);
                    }
                }
                let Saved {
                    snapshot, pending, ..
                } = &mut *saved;
                for edit in pending.iter() {
                    edit.apply(&mut snapshot.state);
                }
            }
            keep_deletes(&mut saved.snapshot.state, &deletes);
            saved.snapshot.state.revocations = verified;
            let first_matches_local = first_meta.is_some_and(|(version, state)| {
                version == saved.snapshot.version && state == saved.snapshot.state
            });
            match push_action(
                peers.len(),
                load_failures.len(),
                loaded,
                peers_agree,
                first_matches_local,
                saved.snapshot.version,
                !saved.pending.is_empty(),
            ) {
                PushAction::UpToDate => {
                    saved.pending.clear();
                    save(data, &saved)?;
                    break;
                }
                PushAction::Skip => {
                    save(data, &saved)?;
                    break;
                }
                PushAction::Push => {
                    let version = saved.snapshot.version;
                    let failures = sync::push_all(&peers, key, &mut saved.snapshot).await?;
                    if saved.snapshot.version > version {
                        saved.pending.clear();
                    }
                    save(data, &saved)?;
                    log("save", &failures);
                    failed.extend(unreached(&connected, &failures));
                    if !failures
                        .iter()
                        .any(|(_, error)| error.is::<VersionConflict>())
                    {
                        break;
                    }
                }
            }
        }
        self.finished.fetch_max(covers, Ordering::SeqCst);
        {
            let current = lock(&self.peers);
            let mut fresh = lock(&self.fresh);
            for (address, _, session) in &connected {
                // A session replaced meanwhile was not loaded by this run.
                let same =
                    (current.get(address)).is_some_and(|(_, now)| now.token == session.token);
                if failed.contains(&address) {
                    // Was fresh from an earlier run, but missed this one (maybe a revocation):
                    // the next resume must retry it.
                    fresh.remove(address);
                } else if same {
                    fresh.insert(address.clone());
                }
            }
        }
        let state = &saved.snapshot.state;
        show(Synced {
            servers: state.servers.clone(),
            settings: state.settings.clone(),
            contacts: state.contacts.clone(),
            conflicts,
        });
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PushAction {
    /// Every reachable peer already holds this exact snapshot and version.
    UpToDate,
    /// No reachable peer and no pending edits, or no connected peers.
    Skip,
    /// Seal and push a new version to peers.
    Push,
}

fn push_action(
    peers: usize,
    load_failures: usize,
    loaded: usize,
    peers_agree: bool,
    first_matches_local: bool,
    local_version: i64,
    has_pending: bool,
) -> PushAction {
    if peers == 0 {
        return PushAction::Skip;
    }
    let reachable = peers.saturating_sub(load_failures);
    if reachable == 0 {
        return if has_pending {
            PushAction::Push
        } else {
            PushAction::Skip
        };
    }
    if loaded == reachable && peers_agree && first_matches_local && local_version > 0 {
        return PushAction::UpToDate;
    }
    PushAction::Push
}

/// Addresses of the connected peers that failed a step, so they are not marked fresh.
fn unreached<'a>(connected: &'a [(Url, Api, Session)], failures: &Failures) -> Vec<&'a Url> {
    (connected.iter())
        .filter(|(_, api, _)| (failures.iter()).any(|(server, _)| std::ptr::eq(*server, api)))
        .map(|(address, ..)| address)
        .collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn save(data: &Path, saved: &Saved) -> Result<()> {
    save_json(data, &data.join("sync.json"), saved).context("save sync state")
}

fn log(step: &str, failures: &Failures) {
    for (server, error) in failures {
        tracing::warn!(
            server = %fingerprint(&server.known_server().server_id),
            step,
            error = %format_args!("{error:#}"),
            "device sync failed on a server"
        );
    }
}

/// The part of the voice and video settings that syncs.
pub fn synced(settings: &Settings) -> BTreeMap<String, Value> {
    let Ok(Value::Object(all)) = serde_json::to_value(settings) else {
        unreachable!("settings serialize to an object");
    };
    all.into_iter()
        .filter(|(key, _)| SYNCED.contains(&key.as_str()))
        .collect()
}

/// `settings` with the synced values from the other devices. Bad values keep the local ones.
pub fn adopt(settings: &Settings, synced: &BTreeMap<String, Value>) -> Settings {
    let mut all = serde_json::to_value(settings).expect("settings serialize");
    for key in SYNCED {
        if let Some(value) = synced.get(key).filter(|value| !value.is_null()) {
            all[key] = value.clone();
        }
    }
    serde_json::from_value(all).unwrap_or_else(|error| {
        tracing::warn!(%error, "synced settings unreadable, keeping this device's");
        settings.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn avatar_cache_survives_restart_but_not_newer_selection_or_account_change() -> Result<()>
    {
        let data = tempfile::tempdir()?;
        let other = tempfile::tempdir()?;
        let sync = DeviceSync::open(data.path())?;
        assert!(sync.avatar(data.path(), &[1; 32], None).await?.is_none());
        assert_eq!(
            sync.avatar(data.path(), &[1; 32], Some(b"synthetic avatar"))
                .await?,
            Some(b"synthetic avatar".to_vec())
        );
        assert_eq!(
            DeviceSync::open(data.path())?
                .avatar(data.path(), &[1; 32], None)
                .await?,
            Some(b"synthetic avatar".to_vec())
        );
        assert!(sync.avatar(data.path(), &[2; 32], None).await?.is_none());
        // A server override must not replace the default, nor cross an account boundary.
        assert_eq!(
            sync.avatar(data.path(), &[1; 32], Some(b"override"))
                .await?,
            Some(b"synthetic avatar".to_vec())
        );
        assert!(
            DeviceSync::open(other.path())?
                .avatar(other.path(), &[2; 32], None)
                .await?
                .is_none()
        );
        sync.edit(
            data.path(),
            vec![Edit::Setting("avatar_digest".into(), Value::Null)],
        )
        .await?;
        assert!(
            sync.avatar(data.path(), &[1; 32], Some(b"synthetic avatar"))
                .await?
                .is_none()
        );
        let mut state = State::default();
        state
            .settings
            .insert("avatar_digest".into(), "newer selection".into());
        Edit::InitialAvatar("old cached selection".into()).apply(&mut state);
        assert_eq!(state.settings["avatar_digest"], "newer selection");
        // Selecting while every server is offline is durable before any upload.
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(8, 8).write_to(&mut png, image::ImageFormat::Png)?;
        sync.select_avatar(data.path(), &[1; 32], png.get_ref())
            .await?;
        assert_eq!(
            DeviceSync::open(data.path())?
                .avatar(data.path(), &[1; 32], None)
                .await?,
            Some(png.into_inner())
        );

        Ok(())
    }

    #[tokio::test]
    async fn avatar_cache_persists_initial_edit_and_ignores_corrupt_base64() -> Result<()> {
        use rusttalk_core::auth::encode;
        let data = tempfile::tempdir()?;
        let person = [3; 32];
        let image = b"synthetic avatar";

        // Seed sync.json with matching cached bytes but no avatar_digest yet.
        let seeded = Saved {
            avatar: Some((person, encode(image))),
            ..Saved::default()
        };
        save(data.path(), &seeded)?;

        let sync = DeviceSync::open(data.path())?;
        assert!(!sync.has_pending().await);
        assert_eq!(
            sync.avatar(data.path(), &person, Some(image)).await?,
            Some(image.to_vec())
        );

        // InitialAvatar must survive reload even though saved.avatar itself did not change.
        let reopened = DeviceSync::open(data.path())?;
        assert!(reopened.has_pending().await);
        assert_eq!(
            reopened.avatar(data.path(), &person, None).await?,
            Some(image.to_vec())
        );

        // Corrupt base64 in saved.avatar must be treated as a cache miss, not an error,
        // and a later valid candidate should repair it.
        let mut corrupt: Saved = crate::load_json(&data.path().join("sync.json"))?;
        corrupt.avatar = Some((person, "!!!not-base64!!!".into()));
        save(data.path(), &corrupt)?;
        let recovered = DeviceSync::open(data.path())?;
        assert_eq!(recovered.avatar(data.path(), &person, None).await?, None);
        assert_eq!(
            recovered.avatar(data.path(), &person, Some(image)).await?,
            Some(image.to_vec())
        );
        Ok(())
    }

    #[tokio::test]
    async fn queued_runs_coalesce_into_one() -> Result<()> {
        use std::sync::{Arc, atomic::AtomicUsize};
        let data = tempfile::tempdir()?;
        let sync = Arc::new(DeviceSync::open(data.path())?);
        let identity = Arc::new(rusttalk_core::identity::DeviceIdentity::create("Test")?.1);
        let shown = Arc::new(AtomicUsize::new(0));
        let run = || {
            let (sync, identity, shown) = (sync.clone(), identity.clone(), shown.clone());
            let data = data.path().to_owned();
            tokio::spawn(async move {
                (sync.run(&data, &identity, Vec::new(), |_| {
                    shown.fetch_add(1, Ordering::SeqCst);
                }))
                .await
            })
        };
        // A slow run in progress: three requests queue behind it.
        let busy = sync.saved.lock().await;
        let queued: Vec<_> = (0..3).map(|_| run()).collect();
        while sync.requested.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
        assert!(!sync.is_current(&"https://10.0.0.1/".parse()?));
        drop(busy);
        for task in queued {
            task.await??;
        }
        assert_eq!(shown.load(Ordering::SeqCst), 1);
        // A later request still runs: it may carry new edits or peers.
        run().await??;
        assert_eq!(shown.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[test]
    fn push_action_skips_version_bump_when_peers_are_up_to_date() {
        assert_eq!(
            push_action(2, 0, 2, true, true, 5, false),
            PushAction::UpToDate
        );
        assert_eq!(
            push_action(2, 0, 2, true, true, 5, true),
            PushAction::UpToDate
        );
        assert_eq!(
            push_action(2, 1, 1, true, true, 5, false),
            PushAction::UpToDate
        );
        assert_eq!(push_action(2, 0, 1, true, true, 5, false), PushAction::Push);
        assert_eq!(
            push_action(2, 0, 2, false, true, 5, false),
            PushAction::Push
        );
        assert_eq!(
            push_action(2, 0, 2, true, false, 5, false),
            PushAction::Push
        );
        assert_eq!(push_action(1, 0, 1, true, true, 0, false), PushAction::Push);
        assert_eq!(
            push_action(0, 0, 0, false, false, 0, true),
            PushAction::Skip
        );
        assert_eq!(
            push_action(1, 1, 0, false, false, 5, false),
            PushAction::Skip
        );
        assert_eq!(
            push_action(1, 1, 0, false, false, 5, true),
            PushAction::Push
        );
    }

    #[test]
    fn devices_stay_local() {
        let local = Settings {
            input: Some("mic".into()),
            output: Some("speakers".into()),
            camera: Some("webcam".into()),
            ..Settings::default()
        };
        let mut other = synced(&local);
        assert!(other.keys().all(|key| SYNCED.contains(&key.as_str())));
        assert!(!other.contains_key("emoji_on_hover"));
        other.insert("push_to_talk".into(), json!(true));
        other.insert("input".into(), json!("their mic"));
        assert!(!other.contains_key("android_terminal_hosting"));
        other.insert("android_terminal_hosting".into(), json!(true));
        let adopted = adopt(&local, &other);
        assert!(adopted.push_to_talk);
        assert!(!adopted.android_terminal_hosting);
        assert_eq!(adopted.input.as_deref(), Some("mic"));
        assert_eq!(adopted.camera.as_deref(), Some("webcam"));

        other.insert("threshold".into(), json!("loud"));
        assert!(adopt(&local, &other) == local);
    }

    #[test]
    fn appearance_follows_to_other_devices() {
        let local = Settings::default();
        let mut other = synced(&local);
        for key in [
            "theme_mode",
            "dark_theme",
            "light_theme",
            "accent",
            "language",
        ] {
            assert!(other.contains_key(key), "{key} is synced");
        }
        other.insert("theme_mode".into(), json!("light"));
        other.insert("light_theme".into(), json!("paper"));
        other.insert("accent".into(), json!("teal"));
        other.insert("language".into(), json!("ja"));
        let adopted = adopt(&local, &other);
        assert_eq!(adopted.language, "ja");
        let look = adopted.appearance();
        assert_eq!(look.mode, crate::ui::Mode::Light);
        assert_eq!(look.light.id, "paper");
        assert_eq!(look.accent.id, "teal");
    }

    #[test]
    fn deletes_published_to_different_servers_both_stay() {
        let url =
            |n: u8| crate::dm::copy(&format!("https://10.0.0.{n}/").parse().unwrap(), &[n; 32]);
        // Device A deleted X while only server 1 was reachable, device B deleted Y
        // while only server 2 was; both published, so neither has a pending edit.
        let mut a = State::default();
        Edit::DeletedDm([1; 32], vec![url(1), url(2)]).apply(&mut a);
        Edit::DmCopyGone([1; 32], url(1)).apply(&mut a);
        let mut b = State::default();
        Edit::DeletedDm([2; 32], vec![url(2)]).apply(&mut b);
        // A loads both copies; B's snapshot wins the settings merge, as in `run`.
        let mut seen = dm_deleted(&a.settings);
        seen.merge(&dm_deleted(&b.settings));
        a.settings = b.settings.clone();
        keep_deletes(&mut a, &seen);
        let deletes = dm_deleted(&a.settings);
        assert!(deletes.contains(&[1; 32]) && deletes.contains(&[2; 32]));
        // Saved and loaded again, like after a restart: still there.
        let saved: BTreeMap<String, Value> =
            serde_json::from_str(&serde_json::to_string(&a.settings).unwrap()).unwrap();
        assert_eq!(dm_deleted(&saved), deletes);
        // Server 1's copy stays gone even if an older snapshot still lists it.
        let mut older = State::default();
        Edit::DeletedDm([1; 32], vec![url(1)]).apply(&mut older);
        keep_deletes(&mut a, &dm_deleted(&older.settings));
        assert!(!dm_deleted(&a.settings).gone(&[1; 32], &url(1)));
    }
}
