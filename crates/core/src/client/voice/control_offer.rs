//! Whether a shared screen takes control requests, so viewers can tell before asking.
//! A participant attribute sealed like the phone consent: LiveKit sees only ciphertext.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{RemoteParticipant, TrackSid, VoiceRoom};
use crate::{
    auth,
    keys::{self, Key},
};

const ATTRIBUTE: &str = "rusttalk.control-offer";
const LABEL: &[u8] = b"rusttalk-control-offer-v1";
const MAX_WIRE: usize = 1024;

/// What a sharer says about control requests for one screen track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlOffer {
    /// Requests are asked about.
    On,
    /// The sharer turned control requests off for this share.
    Off,
    /// This system can't inject input into what it shares.
    Unsupported,
}

#[derive(Serialize, Deserialize)]
struct State {
    track: String,
    offer: ControlOffer,
}

/// Binds the value to its protocol, epoch and sender's device session.
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

fn seal(
    key: &Key,
    epoch: i64,
    (identity, session): (&str, &str),
    track: &str,
    offer: ControlOffer,
) -> Result<String> {
    let state = State {
        track: track.to_owned(),
        offer,
    };
    let sealed = keys::seal(
        key,
        &aad(epoch, identity, session),
        &serde_json::to_vec(&state)?,
    )?;
    Ok(format!("{epoch}.{}", auth::encode(&sealed)))
}

/// The offer for `track`; `None` for another track, a missing key or a bad value.
fn open(
    keys: &HashMap<i64, Key>,
    (identity, session): (&str, &str),
    track: &str,
    value: &str,
) -> Option<ControlOffer> {
    if value.len() > MAX_WIRE {
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
    let state: State = serde_json::from_slice(&clear).ok()?;
    (state.track == track).then_some(state.offer)
}

impl VoiceRoom {
    /// Tell the room whether our screen `track` takes control requests.
    pub async fn announce_control_offer(
        &self,
        track: &TrackSid,
        offer: ControlOffer,
    ) -> Result<()> {
        let participant = self.room.local_participant();
        let key = self
            .watch_keys
            .get(&self.epoch)
            .context("missing control offer key")?;
        let value = seal(
            key,
            self.epoch,
            (self.identity().as_str(), participant.sid().as_str()),
            track.as_str(),
            offer,
        )?;
        participant
            .set_attributes(HashMap::from([(ATTRIBUTE.into(), value)]))
            .await?;
        Ok(())
    }
}

/// What `sharer` says about its screen `track`; `None` from older apps or when unreadable.
pub(super) fn offer(
    keys: &HashMap<i64, Key>,
    sharer: &RemoteParticipant,
    track: &TrackSid,
) -> Option<ControlOffer> {
    let value = sharer.attributes().get(ATTRIBUTE).cloned()?;
    open(
        keys,
        (sharer.identity().as_str(), sharer.sid().as_str()),
        track.as_str(),
        &value,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_round_trips_only_for_its_sender_track_and_key() {
        let key = zeroize::Zeroizing::new([5; 32]);
        let keys = HashMap::from([(3, key.clone())]);
        let me = ("sharer", "PA_session");
        let wire = seal(&key, 3, me, "TR_screen", ControlOffer::Off).unwrap();
        assert_eq!(open(&keys, me, "TR_screen", &wire), Some(ControlOffer::Off));
        let on = seal(&key, 3, me, "TR_screen", ControlOffer::On).unwrap();
        assert_eq!(open(&keys, me, "TR_screen", &on), Some(ControlOffer::On));
        // Another key, sender, session, epoch or track: unknown, as from an older app.
        let wrong = HashMap::from([(3, zeroize::Zeroizing::new([6; 32]))]);
        assert_eq!(open(&wrong, me, "TR_screen", &wire), None);
        assert_eq!(
            open(&keys, ("other", "PA_session"), "TR_screen", &wire),
            None
        );
        assert_eq!(
            open(&keys, ("sharer", "PA_later"), "TR_screen", &wire),
            None
        );
        assert_eq!(
            open(&keys, me, "TR_screen", &wire.replacen("3.", "4.", 1)),
            None
        );
        assert_eq!(open(&keys, me, "TR_next", &wire), None);
        let mut tampered = auth::decode_bytes(wire.split_once('.').unwrap().1).unwrap();
        tampered[30] ^= 1;
        let tampered = format!("3.{}", auth::encode(&tampered));
        assert_eq!(open(&keys, me, "TR_screen", &tampered), None);
        assert_eq!(open(&keys, me, "TR_screen", "garbage"), None);
    }

    #[test]
    fn a_later_kind_of_offer_reads_as_unknown() {
        let key = zeroize::Zeroizing::new([5; 32]);
        let keys = HashMap::from([(1, key.clone())]);
        let clear = br#"{"track":"TR_screen","offer":"maybe"}"#;
        let sealed = keys::seal(&key, &aad(1, "sharer", "PA_s"), clear).unwrap();
        let wire = format!("1.{}", auth::encode(&sealed));
        assert_eq!(open(&keys, ("sharer", "PA_s"), "TR_screen", &wire), None);
    }
}
