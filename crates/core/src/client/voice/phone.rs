//! Explicit, session-bound permission to export microphone audio to an external call.
use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{RemoteParticipant, TrackSid, TrackSource, VoiceRoom, VoiceState};
use crate::{
    auth,
    keys::{self, Key},
};

const ATTRIBUTE: &str = "rusttalk.phone-consent";
const LABEL: &[u8] = b"rusttalk-phone-consent-v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    pub host: String,
    pub session: String,
    pub track: String,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    hosted: Option<String>,
    allowed: Vec<Call>,
}

fn aad(epoch: i64, identity: &str, session: &str) -> Vec<u8> {
    [
        LABEL,
        &epoch.to_be_bytes(),
        identity.as_bytes(),
        b"\0",
        session.as_bytes(),
    ]
    .concat()
}

fn seal(key: &Key, epoch: i64, identity: &str, session: &str, state: &State) -> Result<String> {
    let value = keys::seal(
        key,
        &aad(epoch, identity, session),
        &serde_json::to_vec(state)?,
    )?;
    Ok(format!("{epoch}.{}", auth::encode(&value)))
}

fn open(keys: &HashMap<i64, Key>, identity: &str, session: &str, value: &str) -> Option<State> {
    if value.len() > 60 * 1024 {
        return None;
    }
    let (epoch, value) = value.split_once('.')?;
    let epoch = epoch.parse().ok()?;
    let clear = keys::open(
        keys.get(&epoch)?,
        &aad(epoch, identity, session),
        &auth::decode_bytes(value).ok()?,
    )
    .ok()?;
    serde_json::from_slice(&clear).ok()
}

impl VoiceRoom {
    pub fn local_phone_call(&self, track: &TrackSid) -> Call {
        Call {
            host: self.identity().to_string(),
            session: self.room.local_participant().sid().to_string(),
            track: track.to_string(),
        }
    }

    pub async fn announce_phone(&self, hosted: Option<&TrackSid>, allowed: &[Call]) -> Result<()> {
        let participant = self.room.local_participant();
        let state = State {
            hosted: hosted.map(ToString::to_string),
            allowed: allowed.to_vec(),
        };
        let key = self
            .watch_keys
            .get(&self.epoch)
            .context("missing phone consent key")?;
        let value = seal(
            key,
            self.epoch,
            self.identity().as_str(),
            participant.sid().as_str(),
            &state,
        )?;
        participant
            .set_attributes(HashMap::from([(ATTRIBUTE.into(), value)]))
            .await?;
        Ok(())
    }

    fn phone_state(&self, participant: &RemoteParticipant) -> State {
        participant
            .attributes()
            .get(ATTRIBUTE)
            .and_then(|value| {
                open(
                    &self.watch_keys,
                    participant.identity().as_str(),
                    participant.sid().as_str(),
                    value,
                )
            })
            .unwrap_or_default()
    }

    pub fn external_phone_calls(&self) -> Vec<Call> {
        self.participants()
            .into_iter()
            .filter(|(_, state)| *state == VoiceState::Connected)
            .filter_map(|(p, _)| {
                let track = self.phone_state(&p).hosted?;
                let sid = TrackSid::try_from(track.clone()).ok()?;
                let publication = p.track_publications().get(&sid)?.clone();
                if publication.source() != TrackSource::ScreenshareAudio {
                    return None;
                }
                Some(Call {
                    host: p.identity().to_string(),
                    session: p.sid().to_string(),
                    track,
                })
            })
            .collect()
    }

    /// Only microphones from the exact consenting device session; never app sound.
    pub fn phone_microphones(&self, call: &Call) -> Vec<String> {
        self.participants()
            .into_iter()
            .filter(|(_, state)| *state == VoiceState::Connected)
            .filter(|(p, _)| self.phone_state(p).allowed.contains(call))
            .flat_map(|(p, _)| {
                p.track_publications()
                    .into_values()
                    .filter(|track| track.source() == TrackSource::Microphone)
                    .map(|track| track.sid().to_string())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn consent_is_bound_to_device_session_epoch_and_call() {
        let key = zeroize::Zeroizing::new([3; 32]);
        let keys = HashMap::from([(1, key.clone()), (2, key.clone())]);
        let call = Call {
            host: "host".into(),
            session: "session".into(),
            track: "TR_call".into(),
        };
        let wire = seal(
            &key,
            1,
            "member",
            "device-session",
            &State {
                hosted: None,
                allowed: vec![call.clone()],
            },
        )
        .unwrap();
        assert!(
            open(&keys, "member", "device-session", &wire)
                .unwrap()
                .allowed
                .contains(&call)
        );
        assert!(open(&keys, "other", "device-session", &wire).is_none());
        assert!(open(&keys, "member", "reconnected", &wire).is_none());
        assert!(
            open(
                &keys,
                "member",
                "device-session",
                &wire.replacen("1.", "2.", 1)
            )
            .is_none()
        );
        let next = Call {
            track: "TR_next".into(),
            ..call
        };
        assert!(
            !open(&keys, "member", "device-session", &wire)
                .unwrap()
                .allowed
                .contains(&next)
        );
        let revoked = seal(&key, 1, "member", "device-session", &State::default()).unwrap();
        assert!(
            open(&keys, "member", "device-session", &revoked)
                .unwrap()
                .allowed
                .is_empty()
        );
    }
}
