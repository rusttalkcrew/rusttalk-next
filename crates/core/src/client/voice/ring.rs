//! The ring the others in a call hear when one of them asks someone to join: a data
//! packet sealed like remote control messages, with its own key, to one device each.
//! It says nothing but "ring", so who is asked stays in the DM.

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

fn seal(key: &Key, epoch: i64, from: &str, to: &str) -> Result<Vec<u8>> {
    seal_for(LABEL, MAX_WIRE_BYTES, key, epoch, (from, to), &())
}

fn open(key: &Key, epoch: i64, from: &str, to: &str, wire: &[u8]) -> bool {
    open_for::<()>(LABEL, MAX_WIRE_BYTES, key, epoch, (from, to), wire).is_some()
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
        for to in self.room.remote_participants().into_keys() {
            if self.blocked.contains(to.as_str()) {
                continue;
            }
            let payload = seal(key, self.epoch, from.as_str(), to.as_str())?;
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
        !self.blocked.contains(from.as_str())
            && self.ring_keys.get(&self.epoch).is_some_and(|key| {
                open(
                    key,
                    self.epoch,
                    from.as_str(),
                    self.identity().as_str(),
                    wire,
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
        let wire = seal(&key, 3, "a", "b").unwrap();
        assert!(open(&key, 3, "a", "b", &wire));
        assert!(!open(&key, 4, "a", "b", &wire));
        assert!(!open(&key, 3, "c", "b", &wire));
        assert!(!open(&key, 3, "a", "c", &wire));
        assert!(!open(&Zeroizing::new([8; 32]), 3, "a", "b", &wire));
        assert!(!open(&key, 3, "a", "b", &wire[..wire.len() - 1]));
        assert!(!open(&key, 3, "a", "b", &[0; MAX_WIRE_BYTES + 1]));
    }
}
