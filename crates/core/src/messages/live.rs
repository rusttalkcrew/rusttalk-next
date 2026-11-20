use ed25519_dalek::{Signature, Signer, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{
    auth::{ProtocolError, decode, encode},
    identity::DeviceIdentity,
};

pub const MAX_LIVE_FILE_SIZE: u64 = 2 * 1024 * 1024 * 1024;
pub const LIVE_CHUNK_SIZE: usize = 64 * 1024;

use super::{MAX_FILE_NAME as MAX_NAME, MAX_MEDIA_TYPE as MAX_TYPE};

/// Transfer ID, key, size, chunk size, then length-prefixed name and media type.
pub(super) const MAX_ATTACHMENT: usize = 32 + 32 + 8 + 4 + 1 + MAX_NAME + 1 + MAX_TYPE;

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
    use crate::{
        keys::Key,
        messages::{MessageError, valid_file_name},
    };

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
                || !valid_file_name(&self.name)
                || !(1..=MAX_TYPE).contains(&self.mime_type.len())
                || !self.mime_type.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err(MessageError::Encoding);
            }
            Ok(())
        }

        pub(in crate::messages) fn encode(&self, out: &mut Vec<u8>) -> Result<(), MessageError> {
            self.validate()?;
            out.extend_from_slice(&self.id);
            out.extend_from_slice(self.key.as_ref());
            out.extend_from_slice(&self.size.to_be_bytes());
            out.extend_from_slice(&self.chunk_size.to_be_bytes());
            out.push(self.name.len() as u8);
            out.extend_from_slice(self.name.as_bytes());
            out.push(self.mime_type.len() as u8);
            out.extend_from_slice(self.mime_type.as_bytes());
            Ok(())
        }

        /// Read one offer from the front of `bytes`, leaving the rest.
        pub(in crate::messages) fn decode(bytes: &mut &[u8]) -> Result<Self, MessageError> {
            fn take<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], MessageError> {
                let (head, tail) = bytes.split_first_chunk().ok_or(MessageError::Encoding)?;
                *bytes = tail;
                Ok(*head)
            }
            fn text(bytes: &mut &[u8]) -> Result<String, MessageError> {
                let [length] = take(bytes)?;
                let (text, rest) = bytes
                    .split_at_checked(usize::from(length))
                    .ok_or(MessageError::Encoding)?;
                *bytes = rest;
                String::from_utf8(text.to_vec()).map_err(|_| MessageError::Encoding)
            }

            let attachment = Self {
                id: take(bytes)?,
                key: Key::new(take(bytes)?),
                size: u64::from_be_bytes(take(bytes)?),
                chunk_size: u32::from_be_bytes(take(bytes)?),
                name: text(bytes)?,
                mime_type: text(bytes)?,
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
        request.recipient = encode(&[7; 32]);
        assert!(request.verify(&server, &channel, key).is_err());
        request.recipient = encode(&[3; 32]);
        request.transfer = encode(&[8; 32]);
        assert!(request.verify(&server, &channel, key).is_err());
        request.transfer = encode(&[4; 32]);
        assert!(request.verify(&server, &channel, key).is_ok());
        request.nonce = encode(&[6; 16]);
        assert!(request.verify(&server, &channel, key).is_err());
        request.nonce = encode(&[5; 16]);
        request.cancel = true;
        assert!(request.verify(&server, &channel, key).is_err());
        request.sign(&device, &server, &channel);
        assert!(request.verify(&server, &channel, key).is_ok());
    }

    #[test]
    fn attachment_names_reject_paths_controls_and_bidi_overrides() {
        let file = |name: &str| LiveAttachment {
            id: [1; 32],
            key: crate::keys::Key::new([2; 32]),
            name: name.to_owned(),
            mime_type: "application/octet-stream".to_owned(),
            size: 1,
            chunk_size: LIVE_CHUNK_SIZE as u32,
        };
        for name in ["report.pdf", "...", "naïve 文件.txt", &"a".repeat(MAX_NAME)] {
            assert!(file(name).validate().is_ok(), "{name}");
        }
        let long = "a".repeat(MAX_NAME + 1);
        for name in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "C:x",
            "a\nb",
            "a\0b",
            "invoice\u{202e}fdp.exe",
            "x\u{2066}y",
            "x\u{200f}y",
            &long,
        ] {
            assert!(file(name).validate().is_err(), "{name:?}");
        }
    }
}
