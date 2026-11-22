//! A page channel's document: one signed ciphertext that each save replaces.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::{keys::wire_id, messages::MessageError};

/// Differs from the message domain, so neither can pass as the other.
const DOMAIN: &[u8] = b"rusttalk-page-v1";
/// Format 1's plaintext is the page's Markdown. Apps still read it but save format 2.
const TEXT: u8 = 1;
/// Format 2's is rows of tiles (see `Layout`).
const LAYOUT: u8 = 2;
const NONCE: usize = 24;
const HEADER: usize = 1 + NONCE;
const TAG: usize = 16;
const SIGNATURE: usize = 64;

/// The smallest envelope: an empty page.
pub const MIN_PAGE_BYTES: usize = HEADER + TAG + SIGNATURE;
/// The largest signed envelope.
pub const MAX_PAGE_BYTES: usize = 64 * 1024;
/// The most UTF-8 text that fits in one.
pub const MAX_PAGE_TEXT: usize = MAX_PAGE_BYTES - MIN_PAGE_BYTES;
/// Stored files one page keeps: sixteen pictures, each with its thumbnail.
pub const MAX_KEPT: usize = 32;
/// JSON for the largest envelope (unpadded base64url) and the kept IDs, plus 512 bytes
/// for the numbers.
pub const BODY_LIMIT: usize = (MAX_PAGE_BYTES * 4).div_ceil(3) + MAX_KEPT * 46 + 512;
/// A save may skip versions, to get past one a device saw before the server was restored
/// from a backup, but not so many that one writer could use them all up.
const MAX_STEP: i64 = 10_000;

/// Whether `version` may replace version `base` (0 for no document yet).
pub fn valid_step(base: i64, version: i64) -> bool {
    base >= 0 && version > base && version - base <= MAX_STEP
}

/// Context authenticated by both encryption and the device signature.
#[derive(Clone, Copy)]
pub struct PageContext {
    pub server_id: [u8; 32],
    pub channel_id: [u8; 32],
    pub epoch: i64,
    pub version: i64,
    pub sender_device: [u8; 32],
    pub person: [u8; 32],
}

impl PageContext {
    fn associated_data(self) -> Vec<u8> {
        [
            DOMAIN,
            &self.server_id,
            &self.channel_id,
            &self.epoch.to_be_bytes(),
            &self.version.to_be_bytes(),
            &self.sender_device,
            &self.person,
        ]
        .concat()
    }

    /// Verify the framing and the signature against the sender's device key.
    pub fn verify(self, sender: &VerifyingKey, envelope: &[u8]) -> Result<(), MessageError> {
        if self.epoch <= 0
            || self.version <= 0
            || !(MIN_PAGE_BYTES..=MAX_PAGE_BYTES).contains(&envelope.len())
            || !matches!(envelope[0], TEXT | LAYOUT)
        {
            return Err(MessageError::Encoding);
        }
        if sender.as_bytes() != &self.sender_device {
            return Err(MessageError::Signature);
        }
        let (sealed, signature) = envelope.split_last_chunk().ok_or(MessageError::Encoding)?;
        sender
            .verify_strict(
                &[self.associated_data().as_slice(), sealed].concat(),
                &Signature::from_bytes(signature),
            )
            .map_err(|_| MessageError::Signature)
    }
}

/// A stored document and the fields the server sees.
#[derive(Clone, Serialize, Deserialize)]
pub struct PageRecord {
    pub version: i64,
    pub epoch: i64,
    #[serde(with = "wire_id")]
    pub person: [u8; 32],
    #[serde(with = "wire_id")]
    pub sender_device: [u8; 32],
    pub at: i64,
    pub envelope: String,
}

/// A save: `base` is the version it replaces.
#[derive(Serialize, Deserialize)]
pub struct SavePage {
    pub base: i64,
    pub version: i64,
    pub epoch: i64,
    pub envelope: String,
    /// The stored files the page uses, which the server can't read from the envelope.
    /// They don't expire while this is the page.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep: Vec<String>,
}

#[cfg(any(feature = "client", test))]
mod layout;
#[cfg(any(feature = "client", test))]
pub use layout::{
    Align, Banner, Block, GRID, Layout, MAX_BANNER, MAX_COLUMNS, MAX_PICTURES, MAX_TILES,
    MAX_TITLE, MIN_BANNER, MIN_PICTURE, MIN_WIDTH, Picture, Placed, Row, Tile,
};
#[cfg(any(feature = "client", test))]
mod crypto {
    use chacha20poly1305::{
        KeyInit, XChaCha20Poly1305, XNonce,
        aead::{Aead, Payload},
    };
    use ed25519_dalek::Signer;
    use zeroize::Zeroizing;

    use super::*;
    use crate::{
        identity::{DeviceCertificate, DeviceIdentity, random_bytes},
        keys::Key,
    };

    impl PageContext {
        /// Encrypt a page and sign its envelope with the matching device identity.
        pub fn seal(
            self,
            key: &Key,
            sender: &DeviceIdentity,
            layout: &Layout,
        ) -> Result<Vec<u8>, MessageError> {
            if self.epoch <= 0 || self.version <= 0 {
                return Err(MessageError::Encoding);
            }
            if sender.certificate.signing_key().as_bytes() != &self.sender_device
                || sender.certificate.person_id() != &self.person
            {
                return Err(MessageError::Signature);
            }
            let aad = self.associated_data();
            let plaintext = layout.encode()?;
            let mut nonce = [0; NONCE];
            random_bytes(&mut nonce).map_err(|_| MessageError::Randomness)?;
            let ciphertext = XChaCha20Poly1305::new(&(**key).into())
                .encrypt(
                    &XNonce::from(nonce),
                    Payload {
                        msg: &plaintext,
                        aad: &aad,
                    },
                )
                .map_err(|_| MessageError::Authentication)?;
            let mut envelope = [&[LAYOUT], nonce.as_slice(), &ciphertext].concat();
            let signature = sender
                .signing_key
                .sign(&[aad.as_slice(), &envelope].concat());
            envelope.extend_from_slice(&signature.to_bytes());
            Ok(envelope)
        }

        /// Verify the certified sender's signature before decrypting the page.
        pub fn open(
            self,
            key: &Key,
            sender: &DeviceCertificate,
            envelope: &[u8],
        ) -> Result<Layout, MessageError> {
            if sender.person_id() != &self.person {
                return Err(MessageError::Signature);
            }
            self.verify(sender.signing_key(), envelope)?;
            let aad = self.associated_data();
            let nonce: [u8; NONCE] = envelope[1..HEADER].try_into().expect("verified length");
            let plaintext = Zeroizing::new(
                XChaCha20Poly1305::new(&(**key).into())
                    .decrypt(
                        &XNonce::from(nonce),
                        Payload {
                            msg: &envelope[HEADER..envelope.len() - SIGNATURE],
                            aad: &aad,
                        },
                    )
                    .map_err(|_| MessageError::Authentication)?,
            );
            Layout::decode(envelope[0], &plaintext)
        }
    }
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::Signer;

    use super::*;
    use crate::{
        identity::DeviceIdentity,
        keys::Key,
        messages::{MessageBody, MessageContext},
    };

    fn fixture() -> (DeviceIdentity, Key, PageContext) {
        let (_, sender) = DeviceIdentity::create("editor").unwrap();
        let context = PageContext {
            server_id: [1; 32],
            channel_id: [2; 32],
            epoch: 1,
            version: 1,
            sender_device: sender.certificate.signing_key().to_bytes(),
            person: *sender.certificate.person_id(),
        };
        (sender, Key::new([3; 32]), context)
    }

    #[test]
    fn the_largest_page_fits_and_one_byte_more_does_not() {
        let (sender, key, context) = fixture();
        // The rest is no title, one row and one tile with their lengths, width and kind.
        let text = "x".repeat(MAX_PAGE_TEXT - 8);
        let envelope = context.seal(&key, &sender, &Layout::text(&text)).unwrap();
        assert_eq!(envelope.len(), MAX_PAGE_BYTES);
        assert!(context.open(&key, &sender.certificate, &envelope).unwrap() == Layout::text(&text));
        assert_eq!(
            context
                .seal(&key, &sender, &Layout::text(&format!("{text}x")))
                .err(),
            Some(MessageError::PageLimit)
        );
        let empty = context.seal(&key, &sender, &Layout::default()).unwrap();
        assert_eq!(empty.len(), MIN_PAGE_BYTES + 3);
        assert!(
            context
                .open(&key, &sender.certificate, &empty)
                .unwrap()
                .rows
                .is_empty()
        );
        let body = format!(
            r#"{{"base":{0},"version":{0},"epoch":{0},"envelope":"{1}"}}"#,
            i64::MAX,
            crate::auth::encode(&envelope),
        );
        assert!(body.len() <= BODY_LIMIT);
    }

    #[test]
    fn every_context_field_is_signed_and_encrypted_with() {
        let (sender, key, context) = fixture();
        let envelope = context.seal(&key, &sender, &Layout::text("page")).unwrap();
        for changed in [
            PageContext {
                server_id: [9; 32],
                ..context
            },
            PageContext {
                channel_id: [9; 32],
                ..context
            },
            PageContext {
                epoch: 2,
                ..context
            },
            PageContext {
                version: 2,
                ..context
            },
            PageContext {
                sender_device: [9; 32],
                ..context
            },
            PageContext {
                person: [9; 32],
                ..context
            },
        ] {
            assert_eq!(
                changed.open(&key, &sender.certificate, &envelope).err(),
                Some(MessageError::Signature)
            );
            if changed.sender_device == context.sender_device && changed.person == context.person {
                let (sealed, _) = envelope.split_last_chunk::<SIGNATURE>().unwrap();
                let signed = [changed.associated_data().as_slice(), sealed].concat();
                let signature = sender.signing_key.sign(&signed);
                let resigned = [sealed, &signature.to_bytes()].concat();
                assert_eq!(
                    changed.open(&key, &sender.certificate, &resigned).err(),
                    Some(MessageError::Authentication)
                );
            }
        }
        let mut forged = envelope.clone();
        *forged.last_mut().unwrap() ^= 1;
        assert_eq!(
            context.open(&key, &sender.certificate, &forged).err(),
            Some(MessageError::Signature)
        );
        assert_eq!(
            context
                .open(&Key::new([9; 32]), &sender.certificate, &envelope)
                .err(),
            Some(MessageError::Authentication)
        );
        let mut unknown = envelope;
        unknown[0] = LAYOUT + 1;
        assert_eq!(
            context.open(&key, &sender.certificate, &unknown).err(),
            Some(MessageError::Encoding)
        );
    }

    #[test]
    fn a_page_and_a_message_cannot_pass_as_each_other() {
        let (sender, key, context) = fixture();
        let message = MessageContext {
            server_id: context.server_id,
            channel_id: context.channel_id,
            epoch: context.epoch,
            id: [4; 32],
            sender_device: context.sender_device,
            person: context.person,
        };
        let body = MessageBody {
            text: "text".into(),
            ..Default::default()
        };
        let sent = message.seal(&key, &sender, &body).unwrap();
        assert!(context.open(&key, &sender.certificate, &sent).is_err());
        let page = context.seal(&key, &sender, &Layout::text("text")).unwrap();
        assert!(message.open(&key, &sender.certificate, &page).is_err());
    }

    #[test]
    fn a_save_moves_forward_by_a_bounded_step() {
        assert!(valid_step(0, 1) && valid_step(4, 4 + MAX_STEP));
        for (base, version) in [
            (0, 0),
            (4, 4),
            (4, 3),
            (-1, 1),
            (4, 5 + MAX_STEP),
            (0, i64::MAX),
        ] {
            assert!(!valid_step(base, version), "{base} to {version}");
        }
    }

    #[test]
    fn the_format_byte_is_signed() {
        let (sender, key, context) = fixture();
        let mut envelope = context.seal(&key, &sender, &Layout::text("page")).unwrap();
        assert_eq!(envelope[0], LAYOUT, "text alone must look like any page");
        envelope[0] = TEXT;
        assert_eq!(
            context.open(&key, &sender.certificate, &envelope).err(),
            Some(MessageError::Signature)
        );
    }
}
