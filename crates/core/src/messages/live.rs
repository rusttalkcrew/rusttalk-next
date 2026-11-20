use ed25519_dalek::{Signature, Signer, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{
    auth::{ProtocolError, decode, encode},
    identity::DeviceIdentity,
};

pub const MAX_LIVE_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;
pub const LIVE_CHUNK_SIZE: usize = 64 * 1024;

const MAX_NAME: usize = 1024;
const MAX_TYPE: usize = 127;
pub(super) const MAX_ATTACHMENT: usize = 1 + 32 + 32 + 8 + 4 + 2 + MAX_NAME + 1 + MAX_TYPE;

/// Ephemeral signed request to start or cancel a download, routed to the offering device.
#[derive(Clone, Serialize, Deserialize)]
pub struct LiveFileAcceptance {
    pub recipient: String,
    pub transfer: String,
    pub nonce: String,
    pub cancel: bool,
    pub signature: String,
}

impl LiveFileAcceptance {
    pub fn signed(
        identity: &DeviceIdentity,
        server: &[u8; 32],
        channel: &[u8; 32],
        recipient: &[u8; 32],
        transfer: &[u8; 32],
        nonce: &[u8; 16],
    ) -> Self {
        let mut request = Self {
            recipient: encode(recipient),
            transfer: encode(transfer),
            nonce: encode(nonce),
            cancel: false,
            signature: String::new(),
        };
        request.sign(identity, server, channel);
        request
    }

    pub fn sign(&mut self, identity: &DeviceIdentity, server: &[u8; 32], channel: &[u8; 32]) {
        self.signature = encode(
            &identity
                .signing_key
                .sign(&self.payload(server, channel))
                .to_bytes(),
        );
    }

    pub fn verify(
        &self,
        server: &[u8; 32],
        channel: &[u8; 32],
        device: &VerifyingKey,
    ) -> Result<(), ProtocolError> {
        decode::<32>(&self.recipient)?;
        decode::<32>(&self.transfer)?;
        decode::<16>(&self.nonce)?;
        device
            .verify_strict(
                &self.payload(server, channel),
                &Signature::from_bytes(&decode(&self.signature)?),
            )
            .map_err(|_| ProtocolError::Encoding)
    }

    fn payload(&self, server: &[u8; 32], channel: &[u8; 32]) -> Vec<u8> {
        [
            b"rusttalk-file-accept-v1".as_slice(),
            server,
            channel,
            self.recipient.as_bytes(),
            self.transfer.as_bytes(),
            self.nonce.as_bytes(),
            &[u8::from(self.cancel)],
        ]
        .concat()
    }
}

#[cfg(any(feature = "client", test))]
pub use attachment::LiveAttachment;

#[cfg(any(feature = "client", test))]
mod attachment {
    use super::*;
    use crate::{keys::Key, messages::MessageError};

    /// Private metadata in the signed and encrypted chat offer.
    #[derive(Clone, PartialEq, Eq)]
    pub struct LiveAttachment {
        pub id: [u8; 32],
        pub name: String,
        pub mime_type: String,
        pub size: u64,
        pub chunk_size: u32,
        pub(crate) key: Key,
    }

    impl LiveAttachment {
        pub(crate) fn validate(&self) -> Result<(), MessageError> {
            if self.size > MAX_LIVE_FILE_SIZE
                || self.chunk_size != LIVE_CHUNK_SIZE as u32
                || !(1..=MAX_NAME).contains(&self.name.len())
                || self
                    .name
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
                || matches!(self.name.as_str(), "." | "..")
                || !(1..=MAX_TYPE).contains(&self.mime_type.len())
                || !self.mime_type.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err(MessageError::Encoding);
            }
            Ok(())
        }

        pub(in crate::messages) fn encode(&self, out: &mut Vec<u8>) -> Result<(), MessageError> {
            self.validate()?;
            out.push(1); // live
            out.extend_from_slice(&self.id);
            out.extend_from_slice(self.key.as_ref());
            out.extend_from_slice(&self.size.to_be_bytes());
            out.extend_from_slice(&self.chunk_size.to_be_bytes());
            out.extend_from_slice(&(self.name.len() as u16).to_be_bytes());
            out.extend_from_slice(self.name.as_bytes());
            out.push(self.mime_type.len() as u8);
            out.extend_from_slice(self.mime_type.as_bytes());
            Ok(())
        }

        pub(in crate::messages) fn decode(bytes: &[u8]) -> Result<Self, MessageError> {
            fn take<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], MessageError> {
                let (head, tail) = bytes.split_first_chunk().ok_or(MessageError::Encoding)?;
                *bytes = tail;
                Ok(*head)
            }
            fn text(bytes: &[u8]) -> Result<String, MessageError> {
                String::from_utf8(bytes.to_vec()).map_err(|_| MessageError::Encoding)
            }

            let mut rest = bytes;
            if take(&mut rest)? != [1] {
                return Err(MessageError::Encoding);
            }
            let id = take(&mut rest)?;
            let key = Key::new(take(&mut rest)?);
            let size = u64::from_be_bytes(take(&mut rest)?);
            let chunk_size = u32::from_be_bytes(take(&mut rest)?);
            let length = usize::from(u16::from_be_bytes(take(&mut rest)?));
            let (name, mut rest) = rest
                .split_at_checked(length)
                .ok_or(MessageError::Encoding)?;
            let [length] = take(&mut rest)?;
            if rest.len() != usize::from(length) {
                return Err(MessageError::Encoding);
            }
            let attachment = Self {
                id,
                key,
                size,
                chunk_size,
                name: text(name)?,
                mime_type: text(rest)?,
            };
            attachment.validate()?;
            Ok(attachment)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_signature_binds_channel_transfer_device_and_cancellation() {
        let (_, device) = DeviceIdentity::create("accepting device").unwrap();
        let (_, other) = DeviceIdentity::create("other device").unwrap();
        let server = [1; 32];
        let channel = [2; 32];
        let mut request =
            LiveFileAcceptance::signed(&device, &server, &channel, &[3; 32], &[4; 32], &[5; 16]);
        let key = device.certificate.signing_key();
        assert!(request.verify(&server, &channel, key).is_ok());
        assert!(request.verify(&server, &[9; 32], key).is_err());
        assert!(
            request
                .verify(&server, &channel, other.certificate.signing_key())
                .is_err()
        );
        request.nonce = encode(&[6; 16]);
        assert!(request.verify(&server, &channel, key).is_err());
        request.nonce = encode(&[5; 16]);
        request.cancel = true;
        assert!(request.verify(&server, &channel, key).is_err());
        request.sign(&device, &server, &channel);
        assert!(request.verify(&server, &channel, key).is_ok());
    }
}
