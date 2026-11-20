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

/// Decrypted message text and references.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MessageBody {
    pub text: String,
    pub reply_to: Option<[u8; 32]>,
    pub edit_of: Option<[u8; 32]>,
    pub attachments: Vec<Attachment>,
}

impl MessageBody {
    fn encode(&self) -> Result<Zeroizing<Vec<u8>>, MessageError> {
        if self.text.len() > MAX_TEXT_CHARS * 4 || self.text.chars().count() > MAX_TEXT_CHARS {
            return Err(MessageError::TextLimit);
        }
        if self.attachments.len() > MAX_ATTACHMENTS {
            return Err(MessageError::Attachment);
        }
        let flags = u8::from(self.reply_to.is_some()) | (u8::from(self.edit_of.is_some()) << 1);
        let mut body = Zeroizing::new(Vec::with_capacity(
            MIN_PLAINTEXT
                + 2 * 32
                + self.text.len()
                + self.attachments.len() * MAX_ATTACHMENT_BYTES,
        ));
        body.push(flags);
        body.extend_from_slice(&(self.text.len() as u16).to_be_bytes());
        body.extend_from_slice(self.text.as_bytes());
        body.extend(self.reply_to.iter().flatten());
        body.extend(self.edit_of.iter().flatten());
        body.push(self.attachments.len() as u8);
        for attachment in &self.attachments {
            attachment.encode(&mut body)?;
        }
        Ok(body)
    }

    fn decode(bytes: &[u8]) -> Result<Self, MessageError> {
        let ([flags, high, low], rest) = bytes.split_first_chunk().ok_or(MessageError::Encoding)?;
        if flags & !3 != 0 {
            return Err(MessageError::Encoding);
        }
        let length = u16::from_be_bytes([*high, *low]) as usize;
        let (text, mut tail) = rest
            .split_at_checked(length)
            .ok_or(MessageError::Encoding)?;
        let mut reference = |bit| {
            if *flags & bit != 0 {
                let (id, rest) = tail.split_first_chunk().ok_or(MessageError::Encoding)?;
                tail = rest;
                Ok(Some(*id))
            } else {
                Ok(None)
            }
        };
        let reply_to = reference(1)?;
        let edit_of = reference(2)?;
        let (&count, mut tail) = tail.split_first().ok_or(MessageError::Encoding)?;
        if count as usize > MAX_ATTACHMENTS {
            return Err(MessageError::Attachment);
        }
        let attachments = (0..count)
            .map(|_| Attachment::decode(&mut tail))
            .collect::<Result<_, _>>()?;
        if !tail.is_empty() {
            return Err(MessageError::Encoding);
        }
        let text = std::str::from_utf8(text).map_err(|_| MessageError::Encoding)?;
        if text.chars().count() > MAX_TEXT_CHARS {
            return Err(MessageError::TextLimit);
        }
        Ok(Self {
            text: text.to_owned(),
            reply_to,
            edit_of,
            attachments,
        })
    }
}

impl MessageContext {
    /// Encrypt a plaintext body and sign its envelope with the matching device identity.
    pub fn seal(
        self,
        key: &Key,
        sender: &DeviceIdentity,
        body: &MessageBody,
    ) -> Result<Vec<u8>, MessageError> {
        if self.epoch <= 0 {
            return Err(MessageError::Encoding);
        }
        if sender.certificate.signing_key().as_bytes() != &self.sender_device
            || sender.certificate.person_id() != &self.person
        {
            return Err(MessageError::Signature);
        }
        let aad = self.associated_data();
        let mut nonce = [0; NONCE];
        random_bytes(&mut nonce).map_err(|_| MessageError::Randomness)?;
        let plaintext = body.encode()?;
        let ciphertext = XChaCha20Poly1305::new(&(**key).into())
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| MessageError::Authentication)?;
        let mut envelope = Vec::with_capacity(HEADER + ciphertext.len() + SIGNATURE);
        envelope.push(VERSION);
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&(ciphertext.len() as u16).to_be_bytes());
        envelope.extend_from_slice(&ciphertext);
        let signature = sender
            .signing_key
            .sign(&[aad.as_slice(), &envelope].concat());
        envelope.extend_from_slice(&signature.to_bytes());
        Ok(envelope)
    }

    /// Verify the certified sender's signature before decrypting and decoding the body.
    pub fn open(
        self,
        key: &Key,
        sender: &DeviceCertificate,
        envelope: &[u8],
    ) -> Result<MessageBody, MessageError> {
        if sender.person_id() != &self.person {
            return Err(MessageError::Signature);
        }
        self.verify(sender.signing_key(), envelope)?;
        let aad = self.associated_data();
        let nonce: [u8; NONCE] = envelope[1..1 + NONCE].try_into().expect("verified length");
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
        MessageBody::decode(&plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip_and_context_binding() {
        let (_, sender) = DeviceIdentity::create("sender").unwrap();
        let (_, other) = DeviceIdentity::create("other").unwrap();
        let key = Key::new([1; 32]);
        let context = MessageContext {
            server_id: [2; 32],
            channel_id: [3; 32],
            epoch: 1,
            id: [4; 32],
            sender_device: sender.certificate.signing_key().to_bytes(),
            person: *sender.certificate.person_id(),
        };
        let body = MessageBody {
            text: "🦀".repeat(MAX_TEXT_CHARS),
            reply_to: Some([5; 32]),
            edit_of: Some([6; 32]),
            attachments: Vec::new(),
        };
        let envelope = context.seal(&key, &sender, &body).unwrap();
        assert_eq!(
            envelope.len(),
            MAX_MESSAGE_BYTES - MAX_ATTACHMENTS * MAX_ATTACHMENT_BYTES
        );
        assert!(context.open(&key, &sender.certificate, &envelope).unwrap() == body);
        for changed in [
            MessageContext {
                server_id: [9; 32],
                ..context
            },
            MessageContext {
                channel_id: [9; 32],
                ..context
            },
            MessageContext {
                epoch: 2,
                ..context
            },
            MessageContext {
                id: [9; 32],
                ..context
            },
            MessageContext {
                sender_device: [9; 32],
                ..context
            },
            MessageContext {
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
        assert_eq!(
            context.open(&key, &other.certificate, &envelope).err(),
            Some(MessageError::Signature)
        );
        assert_eq!(
            context
                .open(&Key::new([9; 32]), &sender.certificate, &envelope)
                .err(),
            Some(MessageError::Authentication)
        );

        let mut forged = envelope.clone();
        *forged.last_mut().unwrap() ^= 1;
        assert_eq!(
            context.open(&key, &sender.certificate, &forged).err(),
            Some(MessageError::Signature)
        );
        for malformed in [
            &envelope[..0],
            &envelope[..envelope.len() - 1],
            &[&envelope[..], &[0]].concat(),
        ] {
            assert_eq!(
                context.open(&key, &sender.certificate, malformed).err(),
                Some(MessageError::Encoding)
            );
        }
        let oversized = MessageBody {
            text: format!("{}a", body.text),
            ..body
        };
        assert_eq!(
            context.seal(&key, &sender, &oversized).err(),
            Some(MessageError::TextLimit)
        );
    }

    #[test]
    fn attachment_references_are_bounded() {
        let file = BlobRef {
            id: [1; 32],
            key: Key::new([2; 32]),
            size: 42,
            chunk_size: 65536,
        };
        let thumbnail = BlobRef {
            id: [3; 32],
            ..file.clone()
        };
        let attachment = Attachment {
            file,
            name: "a".repeat(255),
            media_type: "b".repeat(127),
            thumbnail: Some(thumbnail),
        };
        let mut body = MessageBody {
            text: "🦀".repeat(MAX_TEXT_CHARS),
            reply_to: Some([5; 32]),
            edit_of: Some([6; 32]),
            attachments: vec![attachment; MAX_ATTACHMENTS],
        };
        let encoded = body.encode().unwrap();
        assert_eq!(encoded.len(), MAX_PLAINTEXT);
        assert!(MessageBody::decode(&encoded).unwrap() == body);
        assert!(MessageBody::decode(&encoded[..encoded.len() - 1]).is_err());
        body.attachments.push(body.attachments[0].clone());
        assert_eq!(body.encode().err(), Some(MessageError::Attachment));
        assert_eq!(
            MessageBody::decode(&[0, 0, 0, 5]).err(),
            Some(MessageError::Attachment)
        );

        let attachment = encoded.len() - MAX_ATTACHMENT_BYTES;
        for (offset, byte) in [
            (64, 255),  // Oversized file.
            (69, 0),    // Zero chunk size.
            (73, 255),  // Invalid UTF-8.
            (328, 128), // Oversized media type.
            (456, 2),   // Unknown thumbnail flag.
            (521, 255), // Oversized thumbnail.
        ] {
            let mut invalid = encoded.to_vec();
            invalid[attachment + offset] = byte;
            assert!(MessageBody::decode(&invalid).is_err(), "offset {offset}");
        }
    }

    #[test]
    fn plaintext_framing_rejects_bad_lengths_flags_and_text() {
        for bad in [
            &[0, 0, 1, 0][..],
            &[0, 0, 0, 0, 0],
            &[4, 0, 0, 0],
            &[0, 0, 1, 255, 0],
            &[1, 0, 0, 0],
        ] {
            assert_eq!(MessageBody::decode(bad).err(), Some(MessageError::Encoding));
        }
        let mut oversized = vec![0];
        oversized.extend_from_slice(&2001u16.to_be_bytes());
        oversized.extend_from_slice(&[b'x'; 2001]);
        oversized.push(0);
        assert_eq!(
            MessageBody::decode(&oversized).err(),
            Some(MessageError::TextLimit)
        );
    }
}
