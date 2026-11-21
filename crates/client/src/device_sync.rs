//! Device sync (PROMPT 6.4): this person's joined servers and synced settings, merged
//! with the encrypted copy kept on every joined server. Never log settings or addresses.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{Mutex, PoisonError},
};

use anyhow::{Context, Result};
use rusttalk_core::{
    auth::{KnownServer, Session},
    client::{
        Server as Api, save_json,
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

/// Voice and video preferences, dismissed official servers, the appearance and language, that follow the person to their other devices.
/// The microphone, speakers and camera differ per computer and stay in `voice.json` only.
const SYNCED: [&str; 16] = [
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
];

pub struct DeviceSync {
    saved: tokio::sync::Mutex<Saved>,
    /// Connected servers with their current sessions.
    peers: Mutex<HashMap<Url, (Api, Session)>>,
}

/// `sync.json`: the last merged snapshot, so one reachable server can't roll this profile back.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    snapshot: Snapshot,
    /// Local edits no server has accepted yet, applied again after each merge.
    pending: Vec<Edit>,
    /// Servers whose pin conflict was already shown.
    #[serde(skip)]
    warned: HashSet<Url>,
}

/// The synced setting with this person's deleted DM messages (`dm::Tombstones`), so
/// copies on servers that were away get deleted there too and are never shown.
pub const DM_DELETED: &str = "dm_deleted";

#[derive(Serialize, Deserialize)]
pub enum Edit {
    Join(KnownServer),
    Leave(Url),
    Setting(String, Value),
    /// A friend, by hex person ID (see `friends`).
    Contact(String, Value),
    /// One of this person's DM messages, deleted, and servers that may hold a copy.
    /// Added to the other deletes rather than replacing them.
    DeletedDm([u8; 32], Vec<Url>),
    /// That server's copy of a deleted DM message is gone.
    DmCopyGone([u8; 32], Url),
}

impl Edit {
    pub(crate) fn apply(&self, state: &mut State) {
        match self {
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

    pub fn set_peer(&self, address: &Url, peer: Option<(Api, Session)>) {
        let mut peers = self.peers.lock().unwrap_or_else(PoisonError::into_inner);
        match peer {
            Some(peer) => peers.insert(address.clone(), peer),
            None => peers.remove(address),
        };
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
        key: &[u8; 32],
        edits: Vec<Edit>,
        show: impl FnOnce(Synced),
    ) -> Result<()> {
        if !edits.is_empty() {
            self.edit(data, edits).await?;
        }
        let mut saved = self.saved.lock().await;
        let peers: Vec<_> = self
            .peers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        let peers: Vec<sync::Peer> = peers.iter().map(|(api, session)| (api, session)).collect();
        let mut conflicts = Vec::new();
        // A version conflict means another device published in between: load and merge again.
        for _ in 0..3 {
            let (snapshots, failures) = sync::load_all(&peers, key).await;
            log("load", &failures);
            let mut deletes = dm_deleted(&saved.snapshot.state.settings);
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
            let version = saved.snapshot.version;
            let failures = sync::push_all(&peers, key, &mut saved.snapshot).await?;
            if saved.snapshot.version > version {
                saved.pending.clear();
            }
            save(data, &saved)?;
            log("save", &failures);
            if !failures
                .iter()
                .any(|(_, error)| error.is::<VersionConflict>())
            {
                break;
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
        other.insert("push_to_talk".into(), json!(true));
        other.insert("input".into(), json!("their mic"));
        let adopted = adopt(&local, &other);
        assert!(adopted.push_to_talk);
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
        let url = |n: u8| -> Url { format!("https://10.0.0.{n}/").parse().unwrap() };
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
