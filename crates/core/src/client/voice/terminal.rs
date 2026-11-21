//! Terminal sharing on the call's data channel. Messages are sealed like remote control
//! ones, with their own key, to one recipient each. A share is announced as a sealed
//! participant attribute, so people who join later see it too.

use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use livekit::DataPacket;

use super::{
    ParticipantIdentity, RemoteParticipant, VoiceRoom,
    control::{open_for, seal_for},
};
use crate::{
    auth,
    keys::{self, Key},
    terminal::{Message, Share},
};

pub const TOPIC: &str = "rusttalk.terminal";
pub(super) const LABEL: &[u8] = b"rusttalk-terminal-v1";
pub(super) const SHARE_LABEL: &[u8] = b"rusttalk-terminal-share-v1";
const ATTRIBUTE: &str = "rusttalk.terminal";
/// A full output chunk is about 11 KiB sealed; LiveKit takes up to 64 kB.
const MAX_WIRE_BYTES: usize = 15 * 1024;
const MAX_SHARE: usize = 256;

fn share_aad(epoch: i64, sharer: &str) -> Vec<u8> {
    [SHARE_LABEL, &epoch.to_be_bytes(), sharer.as_bytes()].concat()
}

fn seal_share(key: &Key, epoch: i64, sharer: &str, share: &Share) -> Result<String> {
    let clear = serde_json::to_vec(share)?;
    let sealed = keys::seal(key, &share_aad(epoch, sharer), &clear)?;
    Ok(format!("{epoch}.{}", auth::encode(&sealed)))
}

fn open_share(keys: &HashMap<i64, Key>, sharer: &str, value: &str) -> Option<Share> {
    if value.len() > MAX_SHARE {
        return None;
    }
    let (epoch, sealed) = value.split_once('.')?;
    let epoch = epoch.parse().ok()?;
    let sealed = auth::decode_bytes(sealed).ok()?;
    let clear = keys::open(keys.get(&epoch)?, &share_aad(epoch, sharer), &sealed).ok()?;
    serde_json::from_slice(&clear).ok()
}

impl VoiceRoom {
    /// Seal a terminal message and send it to one participant only.
    pub async fn send_terminal(&self, to: &ParticipantIdentity, message: &Message) -> Result<()> {
        ensure!(
            self.can_publish_data(),
            "sending data in this call is denied"
        );
        let key = self
            .terminal_keys
            .get(&self.epoch)
            .context("missing current terminal key")?;
        let from = self.identity();
        let payload = seal_for(
            LABEL,
            MAX_WIRE_BYTES,
            key,
            self.epoch,
            (from.as_str(), to.as_str()),
            message,
        )?;
        self.room
            .local_participant()
            .publish_data(DataPacket {
                payload,
                topic: Some(TOPIC.into()),
                reliable: true,
                destination_identities: vec![to.clone()],
            })
            .await?;
        Ok(())
    }

    /// Open a terminal message `from` a participant, sent to us. `None` if it doesn't
    /// authenticate or isn't a valid message.
    pub fn open_terminal(&self, from: &ParticipantIdentity, wire: &[u8]) -> Option<Message> {
        let key = self.terminal_keys.get(&self.epoch)?;
        let to = self.identity();
        let message: Message = open_for(
            LABEL,
            MAX_WIRE_BYTES,
            key,
            self.epoch,
            (from.as_str(), to.as_str()),
            wire,
        )?;
        message.valid().then_some(message)
    }

    /// Announce our terminal share to the call, or that it stopped.
    pub async fn set_terminal_share(&self, share: Option<&Share>) -> Result<()> {
        let value = match share {
            Some(share) => {
                let key = self
                    .terminal_share_keys
                    .get(&self.epoch)
                    .context("missing terminal share key")?;
                seal_share(key, self.epoch, self.identity().as_str(), share)?
            }
            // An empty value removes the attribute.
            None => String::new(),
        };
        self.room
            .local_participant()
            .set_attributes(HashMap::from([(ATTRIBUTE.into(), value)]))
            .await?;
        Ok(())
    }

    /// The terminal a participant shares, if any.
    pub fn terminal_share(&self, sharer: &RemoteParticipant) -> Option<Share> {
        let value = sharer.attributes().get(ATTRIBUTE)?.clone();
        open_share(
            &self.terminal_share_keys,
            sharer.identity().as_str(),
            &value,
        )
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroizing;

    use super::*;

    #[test]
    fn messages_are_sealed_to_one_sender_and_recipient() {
        let key = Zeroizing::new([7; 32]);
        let message = Message::Data {
            id: [1; 16],
            seq: 3,
            data: vec![b'x'; crate::terminal::MAX_CHUNK],
        };
        let wire = seal_for(LABEL, MAX_WIRE_BYTES, &key, 4, ("a", "b"), &message).unwrap();
        let open = |label: &[u8], from, to| -> Option<Message> {
            open_for(label, MAX_WIRE_BYTES, &key, 4, (from, to), &wire)
        };
        assert_eq!(open(LABEL, "a", "b"), Some(message));
        assert_eq!(open(LABEL, "c", "b"), None);
        assert_eq!(open(LABEL, "a", "c"), None);
        // A control message can't pass for a terminal one, or the other way round.
        assert_eq!(open(super::super::control::LABEL, "a", "b"), None);
    }

    #[test]
    fn the_share_is_bound_to_its_sharer_and_epoch() {
        let key = Zeroizing::new([7; 32]);
        let keys = HashMap::from([(4, key.clone()), (5, key.clone())]);
        let share = Share { id: [3; 16] };
        let value = seal_share(&key, 4, "sharer", &share).unwrap();
        assert_eq!(open_share(&keys, "sharer", &value), Some(share));
        assert_eq!(open_share(&keys, "other", &value), None);
        assert_eq!(
            open_share(&keys, "sharer", &value.replacen("4.", "5.", 1)),
            None
        );
        assert_eq!(open_share(&keys, "sharer", ""), None);
    }
}
