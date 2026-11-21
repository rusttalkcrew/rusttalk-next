//! Remote control messages on the call's data channel, sealed with a key from the
//! call's media key. The media server routes them to one participant but can neither
//! read nor forge them, nor redirect one to someone else.

use anyhow::{Context, Result, ensure};
use livekit::DataPacket;

use super::{ParticipantIdentity, VoiceRoom};
use crate::{
    control::Message,
    keys::{self, Key},
};

pub(super) const TOPIC: &str = "rusttalk.control";
pub(super) const LABEL: &[u8] = b"rusttalk-control-v1";
/// Far more than any message needs.
const MAX_WIRE_BYTES: usize = 1024;

/// Binds a message to its epoch, sender and recipient. Lengths keep the names apart.
fn aad(epoch: i64, from: &str, to: &str) -> Vec<u8> {
    let length = |name: &str| (name.len() as u32).to_be_bytes();
    [
        LABEL,
        &epoch.to_be_bytes(),
        &length(from),
        from.as_bytes(),
        &length(to),
        to.as_bytes(),
    ]
    .concat()
}

/// `epoch || nonce || ciphertext`.
fn seal(key: &Key, epoch: i64, from: &str, to: &str, message: &Message) -> Result<Vec<u8>> {
    let clear = serde_json::to_vec(message)?;
    let sealed = keys::seal(key, &aad(epoch, from, to), &clear)?;
    let wire = [epoch.to_be_bytes().as_slice(), &sealed].concat();
    ensure!(wire.len() <= MAX_WIRE_BYTES, "control message too large");
    Ok(wire)
}

fn open(key: &Key, epoch: i64, from: &str, to: &str, wire: &[u8]) -> Option<Message> {
    if wire.len() > MAX_WIRE_BYTES {
        return None;
    }
    let (stated, sealed) = wire.split_first_chunk::<8>()?;
    // Only the current epoch: a member removed since can't speak with an old key.
    if i64::from_be_bytes(*stated) != epoch {
        return None;
    }
    let clear = keys::open(key, &aad(epoch, from, to), sealed).ok()?;
    let message: Message = serde_json::from_slice(&clear).ok()?;
    message.valid().then_some(message)
}

impl VoiceRoom {
    /// Seal a control message and send it to one participant only.
    pub async fn send_control(&self, to: &ParticipantIdentity, message: &Message) -> Result<()> {
        ensure!(
            self.can_publish_data(),
            "sending data in this call is denied"
        );
        let key = self
            .control_keys
            .get(&self.epoch)
            .context("missing current control key")?;
        let payload = seal(
            key,
            self.epoch,
            self.identity().as_str(),
            to.as_str(),
            message,
        )?;
        self.room
            .local_participant()
            .publish_data(DataPacket {
                payload,
                topic: Some(TOPIC.into()),
                reliable: message.reliable(),
                destination_identities: vec![to.clone()],
            })
            .await?;
        Ok(())
    }

    /// Open a control message `from` a participant, sent to us. `None` if it doesn't
    /// authenticate or isn't a valid message.
    pub fn open_control(&self, from: &ParticipantIdentity, wire: &[u8]) -> Option<Message> {
        let key = self.control_keys.get(&self.epoch)?;
        open(
            key,
            self.epoch,
            from.as_str(),
            self.identity().as_str(),
            wire,
        )
    }
}

#[cfg(test)]
mod tests {
    use zeroize::Zeroizing;

    use super::*;
    use crate::control::{Input, Message};

    #[test]
    fn sealed_to_one_sender_recipient_and_epoch() {
        let key = Zeroizing::new([7; 32]);
        let message = Message::Input {
            session: [1; 16],
            seq: 3,
            input: Input::Key {
                key: "A".into(),
                down: true,
            },
        };
        let wire = seal(&key, 4, "ada", "sharer", &message).unwrap();
        assert_eq!(open(&key, 4, "ada", "sharer", &wire), Some(message));
        // Another sender, recipient, epoch or key: nothing.
        assert_eq!(open(&key, 4, "bob", "sharer", &wire), None);
        assert_eq!(open(&key, 4, "ada", "bob", &wire), None);
        assert_eq!(open(&key, 5, "ada", "sharer", &wire), None);
        assert_eq!(
            open(&Zeroizing::new([8; 32]), 4, "ada", "sharer", &wire),
            None
        );
        // Names can't shift between sender and recipient.
        assert_eq!(open(&key, 4, "ad", "asharer", &wire), None);
        let mut tampered = wire.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(open(&key, 4, "ada", "sharer", &tampered), None);
        assert_eq!(open(&key, 4, "ada", "sharer", &wire[..7]), None);
        assert_eq!(open(&key, 4, "ada", "sharer", &[0; 2000]), None);
        // Authentic but invalid content is still refused.
        let bad = Message::Input {
            session: [1; 16],
            seq: 4,
            input: Input::Move { x: 2.0, y: 0.0 },
        };
        let wire = seal(&key, 4, "ada", "sharer", &bad).unwrap();
        assert_eq!(open(&key, 4, "ada", "sharer", &wire), None);
    }
}
