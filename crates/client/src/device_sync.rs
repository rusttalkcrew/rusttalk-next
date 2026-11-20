//! Device sync (PROMPT 6.4): this person's joined servers and synced settings, merged
//! with the encrypted copy kept on every joined server. Never log settings or addresses.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs, io,
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

use crate::audio::Settings;

/// Voice and video preferences that follow the person to their other devices.
/// The microphone, speakers and camera differ per computer and stay in `voice.json` only.
const SYNCED: [&str; 8] = [
    "push_to_talk",
    "key",
    "threshold",
    "echo_cancellation",
    "noise_suppression",
    "auto_gain",
    "camera_height",
    "camera_fps",
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

#[derive(Serialize, Deserialize)]
pub enum Edit {
    Join(KnownServer),
    Leave(Url),
    Setting(String, Value),
}

impl Edit {
    fn apply(&self, state: &mut State) {
        match self {
            Self::Join(server) => state.join(server.clone()),
            Self::Leave(address) => state.leave(address),
            Self::Setting(key, value) => {
                state.settings.insert(key.clone(), value.clone());
            }
        }
    }
}

/// The merged state for the UI.
pub struct Synced {
    pub servers: Vec<KnownServer>,
    pub settings: BTreeMap<String, Value>,
    /// Servers another device pinned differently. They are never adopted.
    pub conflicts: Vec<Url>,
}

impl DeviceSync {
    pub fn open(data: &Path) -> Result<Self> {
        let saved = match fs::read(data.join("sync.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("read sync state")?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Saved::default(),
            Err(error) => return Err(error).context("open sync state"),
        };
        Ok(Self {
            saved: tokio::sync::Mutex::new(saved),
            peers: Mutex::default(),
        })
    }

    /// Joined servers, to reconnect at start. Not for async code.
    pub fn servers(&self) -> Vec<KnownServer> {
        self.saved.blocking_lock().snapshot.state.servers.clone()
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
            let (remote, failures) = sync::load(&peers, key).await;
            log("load", &failures);
            if let Some(remote) = remote {
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
}
