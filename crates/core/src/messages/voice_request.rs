//! Asking someone to join a voice channel, and their answer, inside the encrypted
//! body of a DM message.

use super::MessageError;

/// Format 3's kind byte for both, after the poll parts' 1 to 3.
pub(super) const KIND: u8 = 4;
const ASK: u8 = 0;

/// A request to join a voice channel, or the answer to one; sent instead of chat text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceRequest {
    /// Join this voice channel, on the server that carries the DM message.
    Ask([u8; 32]),
    /// The answer to the request with this message ID.
    Answer([u8; 32], VoiceAnswer),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceAnswer {
    Joining = 1,
    Declined = 2,
    Later = 3,
}

impl VoiceRequest {
    pub(super) fn encode(self, body: &mut Vec<u8>) {
        let (action, id) = match self {
            Self::Ask(channel) => (ASK, channel),
            Self::Answer(request, answer) => (answer as u8, request),
        };
        body.extend_from_slice(&[KIND, action]);
        body.extend_from_slice(&id);
    }

    /// What follows the kind byte.
    pub(super) fn decode(tail: &mut &[u8]) -> Result<Self, MessageError> {
        let ([action, id @ ..], rest) = tail
            .split_first_chunk::<33>()
            .ok_or(MessageError::Encoding)?;
        let id = *id;
        let request = match *action {
            ASK => Self::Ask(id),
            1 => Self::Answer(id, VoiceAnswer::Joining),
            2 => Self::Answer(id, VoiceAnswer::Declined),
            3 => Self::Answer(id, VoiceAnswer::Later),
            _ => return Err(MessageError::Encoding),
        };
        *tail = rest;
        Ok(request)
    }
}
