//! The ring the others in a call hear when one of them asks someone to join: a data
//! packet sealed like remote control messages, with its own key, to one device each.
//! It says nothing but "ring", so who is asked stays in the DM.

use std::sync::Mutex;

use anyhow::{Context, Result, ensure};
use livekit::DataPacket;

use super::{
    ParticipantIdentity, VoiceRoom,
    control::{open_for, seal_for},
};
use crate::keys::Key;

pub const TOPIC: &str = "rusttalk.ring";
pub(super) const LABEL: &[u8] = b"rusttalk-ring-v1";
const MAX_WIRE_BYTES: usize = 128;

const FRESH_MS: i64 = 30_000;
const MAX_SEEN: usize = 256;

// Keep accepted packets across room reconnects until their authenticated time expires.
static SEEN: Mutex<Vec<(i64, Vec<u8>)>> = Mutex::new(Vec::new());

fn seal(key: &Key, epoch: i64, from: &str, to: &str, sent: i64) -> Result<Vec<u8>> {
    seal_for(LABEL, MAX_WIRE_BYTES, key, epoch, (from, to), &sent)
}

fn open(
    key: &Key,
    epoch: i64,
    from: &str,
    to: &str,
    wire: &[u8],
    now: i64,
    seen: &mut Vec<(i64, Vec<u8>)>,
) -> bool {
    let Some(sent) = open_for::<i64>(LABEL, MAX_WIRE_BYTES, key, epoch, (from, to), wire) else {
        return false;
    };
    if !(0..FRESH_MS).contains(&now.saturating_sub(sent)) {
        return false;
    }
    seen.retain(|(at, _)| now.saturating_sub(*at) < FRESH_MS);
    if seen.len() >= MAX_SEEN || seen.iter().any(|(_, packet)| packet == wire) {
        return false;
    }
    seen.push((sent, wire.to_vec()));
    true
}

impl VoiceRoom {
    /// Ring every other device in the call.
    pub async fn send_ring(&self) -> Result<()> {
        ensure!(
            self.can_publish_data(),
            "sending data in this call is denied"
        );
        let key = self
            .ring_keys
            .get(&self.epoch)
            .context("missing current ring key")?;
        let from = self.identity();
        let sent = crate::index::now_ms().context("ring clock unavailable")?;
        for to in self.room.remote_participants().into_keys() {
            if self.blocked.contains(to.as_str()) {
                continue;
            }
            let payload = seal(key, self.epoch, from.as_str(), to.as_str(), sent)?;
            self.room
                .local_participant()
                .publish_data(DataPacket {
                    payload,
                    topic: Some(TOPIC.into()),
                    reliable: true,
                    destination_identities: vec![to],
                })
                .await?;
        }
        Ok(())
    }

    /// Whether `wire` is a ring `from` a participant, sent to us.
    pub fn open_ring(&self, from: &ParticipantIdentity, wire: &[u8]) -> bool {
        let Some(now) = crate::index::now_ms() else {
            return false;
        };
        !self.blocked.contains(from.as_str())
            && self.ring_keys.get(&self.epoch).is_some_and(|key| {
                open(
                    key,
                    self.epoch,
                    from.as_str(),
                    self.identity().as_str(),
                    wire,
                    now,
                    &mut SEEN.lock().unwrap_or_else(|e| e.into_inner()),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroizing;

    use super::*;

    #[test]
    fn a_ring_opens_only_for_its_sender_recipient_and_epoch() {
        let key = Zeroizing::new([7; 32]);
        let wire = seal(&key, 3, "a", "b", 100).unwrap();
        let mut seen = Vec::new();
        assert!(!open(&key, 3, "a", "b", &wire, 99, &mut seen));
        assert!(!open(&key, 3, "a", "b", &wire, 100 + FRESH_MS, &mut seen));
        for (epoch, from, to) in [(4, "a", "b"), (3, "c", "b"), (3, "a", "c")] {
            assert!(!open(&key, epoch, from, to, &wire, 100, &mut seen));
        }
        assert!(!open(
            &Zeroizing::new([8; 32]),
            3,
            "a",
            "b",
            &wire,
            100,
            &mut seen
        ));
        assert!(!open(
            &key,
            3,
            "a",
            "b",
            &wire[..wire.len() - 1],
            100,
            &mut seen
        ));
        assert!(!open(
            &key,
            3,
            "a",
            "b",
            &[0; MAX_WIRE_BYTES + 1],
            100,
            &mut seen
        ));
        assert!(open(&key, 3, "a", "b", &wire, 100, &mut seen));
        assert!(!open(&key, 3, "a", "b", &wire, 4100, &mut seen));
        seen.resize(MAX_SEEN, (100, Vec::new()));
        let fresh = seal(&key, 3, "a", "b", 101).unwrap();
        assert!(!open(&key, 3, "a", "b", &fresh, 101, &mut seen));
        assert!(open(&key, 3, "a", "b", &fresh, 100 + FRESH_MS, &mut seen));
    }
}
