use std::collections::HashMap;

use anyhow::{Context, Result, ensure};

use super::{RemoteParticipant, TrackSid, VoiceRoom};
use crate::{
    auth,
    keys::{self, Key},
};

const ATTRIBUTE: &str = "rusttalk.video-watches";
pub(super) const LABEL: &[u8] = b"rusttalk-video-watches-v1";
// LiveKit limits the combined size of a participant's attributes to 64 KiB.
const MAX_WIRE: usize = 60 * 1024;

fn aad(epoch: i64, viewer: &str) -> Vec<u8> {
    [LABEL, &epoch.to_be_bytes(), viewer.as_bytes()].concat()
}

fn seal(key: &Key, epoch: i64, viewer: &str, tracks: &[TrackSid]) -> Result<String> {
    let clear = serde_json::to_vec(&tracks.iter().map(TrackSid::as_str).collect::<Vec<_>>())?;
    let sealed = keys::seal(key, &aad(epoch, viewer), &clear)?;
    let value = format!("{epoch}.{}", auth::encode(&sealed));
    ensure!(value.len() <= MAX_WIRE, "video watch state too large");
    Ok(value)
}

fn open(keys: &HashMap<i64, Key>, viewer: &str, value: &str) -> Option<Vec<TrackSid>> {
    if value.len() > MAX_WIRE {
        return None;
    }
    let (epoch, sealed) = value.split_once('.')?;
    let epoch = epoch.parse().ok()?;
    let sealed = auth::decode_bytes(sealed).ok()?;
    let clear = crate::keys::open(keys.get(&epoch)?, &aad(epoch, viewer), &sealed).ok()?;
    let tracks: Vec<String> = serde_json::from_slice(&clear).ok()?;
    Some(
        tracks
            .into_iter()
            .filter_map(|sid| TrackSid::try_from(sid).ok())
            .collect(),
    )
}

impl VoiceRoom {
    /// Announce current video subscriptions, encrypted for members of this call.
    pub async fn set_video_watches(&self, tracks: &[TrackSid]) -> Result<()> {
        let key = self
            .watch_keys
            .get(&self.epoch)
            .context("missing video watch key")?;
        let value = seal(key, self.epoch, self.identity().as_str(), tracks)?;
        self.room
            .local_participant()
            .set_attributes(HashMap::from([(ATTRIBUTE.into(), value)]))
            .await?;
        Ok(())
    }

    /// Open a viewer's latest subscriptions. The sender identity is authenticated
    /// so a relay cannot attach one device's snapshot to another device.
    pub fn video_watches(&self, viewer: &RemoteParticipant) -> Vec<TrackSid> {
        watches(&self.watch_keys, viewer)
    }
}

/// A viewer's latest subscriptions; empty until the key of their epoch is here.
pub(super) fn watches(keys: &HashMap<i64, Key>, viewer: &RemoteParticipant) -> Vec<TrackSid> {
    viewer
        .attributes()
        .get(ATTRIBUTE)
        .and_then(|value| open(keys, viewer.identity().as_str(), value))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_state_is_bound_to_its_viewer_and_epoch() {
        let key = zeroize::Zeroizing::new([7; 32]);
        let keys = HashMap::from([(4, key.clone()), (5, key.clone())]);
        let tracks = vec![TrackSid::try_from("TR_screen".to_owned()).unwrap()];
        let wire = seal(&key, 4, "viewer", &tracks).unwrap();
        assert_eq!(open(&keys, "viewer", &wire), Some(tracks));
        assert_eq!(open(&keys, "other", &wire), None);
        assert_eq!(open(&keys, "viewer", &wire.replacen("4.", "5.", 1)), None);
        let empty = seal(&key, 4, "viewer", &[]).unwrap();
        assert_eq!(open(&keys, "viewer", &empty), Some(Vec::new()));
    }
}
