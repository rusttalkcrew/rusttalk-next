//! Explicit, session-bound permission to export microphone audio to an external call.
use std::{
    collections::HashMap,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{RemoteParticipant, TrackSid, TrackSource, VoiceRoom, VoiceState};
use crate::{
    auth,
    keys::{self, Key},
};

const ATTRIBUTE: &str = "rusttalk.phone-consent";
const LABEL: &[u8] = b"rusttalk-phone-consent-v2";
const ACK_ATTRIBUTE: &str = "rusttalk.phone-withdrawal-ack";
const ACK_LABEL: &[u8] = b"rusttalk-phone-withdrawal-ack-v2";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    pub host: String,
    pub session: String,
    pub track: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct State {
    revision: u64,
    withdrawn: Vec<Call>,
    hosted: Option<String>,
    allowed: Vec<Call>,
}

fn aad(label: &[u8], epoch: i64, identity: &str, session: &str) -> Vec<u8> {
    [
        label,
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
        &aad(LABEL, epoch, identity, session),
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
        &aad(LABEL, epoch, identity, session),
        &auth::decode_bytes(value).ok()?,
    )
    .ok()?;
    let state: State = serde_json::from_slice(&clear).ok()?;
    (state.revision > 0).then_some(state)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Acknowledgement {
    member: String,
    session: String,
    revision: u64,
    call: Call,
}

/// Lives as long as the room, including signaling reconnects and key refreshes.
#[derive(Default)]
pub(super) struct Consent {
    revision: AtomicU64,
    seen: Mutex<HashMap<(String, String), State>>,
}

impl Consent {
    fn accept(&self, identity: &str, session: &str, state: Option<State>) -> State {
        let Some(state) = state else {
            return State::default();
        };
        let mut seen = self.seen.lock().expect("phone consent freshness");
        let key = (identity.to_owned(), session.to_owned());
        if let Some(previous) = seen.get(&key)
            && (state.revision < previous.revision
                || state.revision == previous.revision && state != *previous)
        {
            return State::default();
        }
        seen.insert(key, state.clone());
        state
    }
}

impl VoiceRoom {
    pub fn local_phone_call(&self, track: &TrackSid) -> Call {
        Call {
            host: self.identity().to_string(),
            session: self.room.local_participant().sid().to_string(),
            track: track.to_string(),
        }
    }

    pub async fn announce_phone(
        &self,
        hosted: Option<&TrackSid>,
        allowed: &[Call],
        withdrawn: &[Call],
    ) -> Result<u64> {
        let participant = self.room.local_participant();
        let revision = self
            .phone_consent
            .revision
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
            .map_err(|_| anyhow::anyhow!("phone consent revision exhausted"))?
            + 1;
        let state = State {
            revision,
            withdrawn: withdrawn.to_vec(),
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
        Ok(revision)
    }

    fn phone_state(&self, participant: &RemoteParticipant) -> State {
        let state = participant.attributes().get(ATTRIBUTE).and_then(|value| {
            open(
                &self.watch_keys,
                participant.identity().as_str(),
                participant.sid().as_str(),
                value,
            )
        });
        self.phone_consent.accept(
            participant.identity().as_str(),
            participant.sid().as_str(),
            state,
        )
    }

    /// A single consent snapshot drives both mixer filtering and acknowledgements.
    /// Apply the microphones first, then publish these acknowledgements.
    pub fn phone_bridge_state(&self, call: &Call) -> (Vec<String>, Vec<Acknowledgement>) {
        let mut microphones = Vec::new();
        let mut acknowledgements = Vec::new();
        for (p, connection) in self.participants() {
            let state = self.phone_state(&p);
            if state.allowed.contains(call) && connection == VoiceState::Connected {
                microphones.extend(
                    p.track_publications()
                        .into_values()
                        .filter(|track| track.source() == TrackSource::Microphone)
                        .map(|track| track.sid().to_string()),
                );
            } else if state.withdrawn.contains(call) && !state.allowed.contains(call) {
                acknowledgements.push(Acknowledgement {
                    member: p.identity().to_string(),
                    session: p.sid().to_string(),
                    revision: state.revision,
                    call: call.clone(),
                });
            }
        }
        acknowledgements.sort_by(|a, b| (&a.member, &a.session).cmp(&(&b.member, &b.session)));
        (microphones, acknowledgements)
    }

    pub async fn acknowledge_phone_withdrawals(
        &self,
        acknowledgements: &[Acknowledgement],
    ) -> Result<()> {
        let local = self.room.local_participant();
        let key = self
            .watch_keys
            .get(&self.epoch)
            .context("missing phone consent key")?;
        let sealed = keys::seal(
            key,
            &aad(
                ACK_LABEL,
                self.epoch,
                self.identity().as_str(),
                local.sid().as_str(),
            ),
            &serde_json::to_vec(acknowledgements)?,
        )?;
        let value = format!("{}.{}", self.epoch, auth::encode(&sealed));
        local
            .set_attributes(HashMap::from([(ACK_ATTRIBUTE.into(), value)]))
            .await?;
        Ok(())
    }

    pub fn phone_withdrawal_acknowledged(&self, call: &Call, revision: u64) -> bool {
        self.participants().into_iter().any(|(p, _)| {
            if p.identity().as_str() != call.host || p.sid().as_str() != call.session {
                return false;
            }
            let attributes = p.attributes();
            let Some(value) = attributes
                .get(ACK_ATTRIBUTE)
                .filter(|v| v.len() <= 60 * 1024)
            else {
                return false;
            };
            let opened = (|| {
                let (epoch, value) = value.split_once('.')?;
                let epoch = epoch.parse().ok()?;
                let clear = keys::open(
                    self.watch_keys.get(&epoch)?,
                    &aad(ACK_LABEL, epoch, p.identity().as_str(), p.sid().as_str()),
                    &auth::decode_bytes(value).ok()?,
                )
                .ok()?;
                serde_json::from_slice::<Vec<Acknowledgement>>(&clear).ok()
            })();
            opened.is_some_and(|acks| {
                acks.iter().any(|ack| {
                    ack.call == *call
                        && ack.member == self.identity().as_str()
                        && ack.session == self.room.local_participant().sid().as_str()
                        && ack.revision >= revision
                })
            })
        })
    }

    /// Missing/invalid consent metadata is not evidence that the bridge ended.
    pub fn phone_call_ended(&self, call: &Call) -> bool {
        if self.connection_state() != super::ConnectionState::Connected {
            return false;
        }
        !self.room.remote_participants().into_values().any(|p| {
            p.identity().as_str() == call.host
                && p.sid().as_str() == call.session
                && p.track_publications()
                    .keys()
                    .any(|sid| sid.as_str() == call.track)
        })
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
        self.phone_bridge_state(call).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn authenticated_revision_survives_replay_key_refresh_and_reconnect() {
        let key = zeroize::Zeroizing::new([3; 32]);
        let keys = HashMap::from([(1, key.clone()), (2, key.clone())]);
        let consent = Consent::default();
        let call = Call {
            host: "host".into(),
            session: "host-session".into(),
            track: "TR_call".into(),
        };
        let grant = State {
            revision: 1,
            allowed: vec![call.clone()],
            ..Default::default()
        };
        let withdrawal = State {
            revision: 2,
            withdrawn: vec![call.clone()],
            ..Default::default()
        };
        let first = seal(&key, 1, "member", "session", &grant).unwrap();
        let stop = seal(&key, 1, "member", "session", &withdrawal).unwrap();
        assert_eq!(
            consent.accept(
                "member",
                "session",
                open(&keys, "member", "session", &first)
            ),
            grant
        );
        assert_eq!(
            consent.accept("member", "session", open(&keys, "member", "session", &stop)),
            withdrawal
        );
        assert!(
            consent
                .accept(
                    "member",
                    "session",
                    open(&keys, "member", "session", &first)
                )
                .allowed
                .is_empty()
        );
        // A key refresh must not reset the session's high-water mark.
        let old_under_new_key = seal(&key, 2, "member", "session", &grant).unwrap();
        assert!(
            consent
                .accept(
                    "member",
                    "session",
                    open(&keys, "member", "session", &old_under_new_key)
                )
                .allowed
                .is_empty()
        );
        // Same-revision conflicting content and legacy states fail closed.
        let conflicting = State {
            revision: 2,
            ..grant.clone()
        };
        let wire = seal(&key, 1, "member", "session", &conflicting).unwrap();
        assert!(
            consent
                .accept("member", "session", open(&keys, "member", "session", &wire))
                .allowed
                .is_empty()
        );
        assert!(consent.accept("member", "session", None).allowed.is_empty());
        assert!(
            consent
                .accept(
                    "member",
                    "session",
                    open(&keys, "member", "session", &first)
                )
                .allowed
                .is_empty()
        );
        assert!(open(&keys, "member", "new-session", &first).is_none());
        let legacy = seal(&key, 1, "member", "session", &State::default()).unwrap();
        assert!(open(&keys, "member", "session", &legacy).is_none());
        let fresh = State {
            revision: 3,
            ..grant
        };
        let fresh_wire = seal(&key, 2, "member", "session", &fresh).unwrap();
        assert_eq!(
            consent.accept(
                "member",
                "session",
                open(&keys, "member", "session", &fresh_wire)
            ),
            fresh
        );
        let mut tampered = auth::decode_bytes(fresh_wire.split_once('.').unwrap().1).unwrap();
        tampered[0] ^= 1;
        assert!(
            open(
                &keys,
                "member",
                "session",
                &format!("2.{}", auth::encode(&tampered))
            )
            .is_none()
        );
    }

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
                revision: 1,
                withdrawn: Vec::new(),
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
        let revoked = seal(
            &key,
            1,
            "member",
            "device-session",
            &State {
                revision: 2,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            open(&keys, "member", "device-session", &revoked)
                .unwrap()
                .allowed
                .is_empty()
        );
    }
}
