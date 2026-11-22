//! Server events: one document per event, sealed with the space key and signed by its
//! author's device. The server sees only who made it and when to delete it.

use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use ed25519_dalek::{Signature, Signer, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::{
    identity::{DeviceCertificate, DeviceIdentity, random_bytes},
    keys::{Key, wire_id},
};

/// Differs from the message and metadata domains, so none can pass as another.
const DOMAIN: &[u8] = b"rusttalk-event-v1";
const FORMAT: u8 = 1;
const NONCE: usize = 24;
const HEADER: usize = 1 + NONCE;
const TAG: usize = 16;
const SIGNATURE: usize = 64;
/// Start, end, the kind of place and the title's length.
const FIELDS: usize = 8 + 8 + 1 + 2;
const NOWHERE: u8 = 0;
const IRL: u8 = 1;
const CHANNEL: u8 = 2;

/// How long an event stays after its end, in seconds.
pub const GRACE: i64 = 2 * 60 * 60;
/// How far ahead an event can end, in seconds.
pub const HORIZON: i64 = 366 * 24 * 60 * 60;
pub const MAX_TITLE: usize = 80;
pub const MAX_DESCRIPTION: usize = 500;
/// A place in real life: an address or how to get there.
pub const MAX_PLACE: usize = 200;
pub const MIN_EVENT_BYTES: usize = HEADER + FIELDS + TAG + SIGNATURE;
/// Every character may take four UTF-8 bytes; a place in real life is longer than a channel ID.
pub const MAX_EVENT_BYTES: usize =
    MIN_EVENT_BYTES + 4 * (MAX_TITLE + MAX_DESCRIPTION) + 2 + 4 * MAX_PLACE;
/// JSON for the largest event (unpadded base64url) and its other fields.
pub const BODY_LIMIT: usize = (MAX_EVENT_BYTES * 4).div_ceil(3) + 512;

#[derive(Debug, Error)]
pub enum EventError {
    #[error("invalid event encoding")]
    Encoding,
    #[error("invalid event signature or author")]
    Signature,
    #[error("event encryption or authentication failed")]
    Authentication,
    #[error("invalid event title, description, place or times")]
    Invalid,
    #[error("event randomness unavailable")]
    Randomness,
}

/// What a person enters. Times are Unix seconds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    pub title: String,
    pub description: String,
    pub start: i64,
    pub end: i64,
    pub place: Place,
}

/// Where an event happens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Place {
    #[default]
    Nowhere,
    /// In real life: an address or a short note, up to `MAX_PLACE` characters.
    Irl(String),
    /// A channel of this server, by ID. Apps show its name, if they can see it.
    Channel([u8; 32]),
}

impl Details {
    /// When the server deletes this event.
    pub fn remove_at(&self) -> Result<i64, EventError> {
        self.end.checked_add(GRACE).ok_or(EventError::Invalid)
    }

    fn check(&self) -> Result<(), EventError> {
        if self.title.trim().is_empty()
            || self.title.chars().count() > MAX_TITLE
            || self.description.chars().count() > MAX_DESCRIPTION
            || matches!(&self.place, Place::Irl(text) if text.trim().is_empty() || text.chars().count() > MAX_PLACE)
            || self.start <= 0
            || self.end <= self.start
        {
            return Err(EventError::Invalid);
        }
        Ok(())
    }

    fn encode(&self) -> Result<Zeroizing<Vec<u8>>, EventError> {
        self.check()?;
        let title = u16::try_from(self.title.len()).map_err(|_| EventError::Invalid)?;
        let mut out = Zeroizing::new(Vec::with_capacity(
            FIELDS + 2 + 4 * MAX_PLACE + self.title.len() + self.description.len(),
        ));
        out.extend_from_slice(&self.start.to_be_bytes());
        out.extend_from_slice(&self.end.to_be_bytes());
        match &self.place {
            Place::Nowhere => out.push(NOWHERE),
            Place::Irl(text) => {
                out.push(IRL);
                let length = u16::try_from(text.len()).map_err(|_| EventError::Invalid)?;
                out.extend_from_slice(&length.to_be_bytes());
                out.extend_from_slice(text.as_bytes());
            }
            Place::Channel(id) => {
                out.push(CHANNEL);
                out.extend_from_slice(id);
            }
        }
        out.extend_from_slice(&title.to_be_bytes());
        out.extend_from_slice(self.title.as_bytes());
        out.extend_from_slice(self.description.as_bytes());
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self, EventError> {
        let (start, rest) = bytes.split_first_chunk().ok_or(EventError::Encoding)?;
        let (end, rest) = rest.split_first_chunk().ok_or(EventError::Encoding)?;
        let text = |part: &[u8]| String::from_utf8(part.to_vec()).map_err(|_| EventError::Encoding);
        let (kind, rest) = rest.split_first().ok_or(EventError::Encoding)?;
        let (place, rest) = match *kind {
            NOWHERE => (Place::Nowhere, rest),
            IRL => {
                let (length, rest) = rest.split_first_chunk().ok_or(EventError::Encoding)?;
                let (place, rest) = (rest.split_at_checked(u16::from_be_bytes(*length).into()))
                    .ok_or(EventError::Encoding)?;
                (Place::Irl(text(place)?), rest)
            }
            CHANNEL => {
                let (id, rest) = rest.split_first_chunk().ok_or(EventError::Encoding)?;
                (Place::Channel(*id), rest)
            }
            _ => return Err(EventError::Encoding),
        };
        let (length, rest) = rest.split_first_chunk().ok_or(EventError::Encoding)?;
        let (title, description) = rest
            .split_at_checked(u16::from_be_bytes(*length).into())
            .ok_or(EventError::Encoding)?;
        let details = Self {
            title: text(title)?,
            description: text(description)?,
            start: i64::from_be_bytes(*start),
            end: i64::from_be_bytes(*end),
            place,
        };
        details.check()?;
        Ok(details)
    }
}

/// Context authenticated by both encryption and the device signature.
#[derive(Clone, Copy)]
pub struct EventContext {
    pub server_id: [u8; 32],
    pub id: [u8; 32],
    pub epoch: i64,
    pub remove_at: i64,
    pub author_device: [u8; 32],
    pub author: [u8; 32],
}

impl EventContext {
    fn associated_data(self) -> Vec<u8> {
        [
            DOMAIN,
            &self.server_id,
            &self.id,
            &self.epoch.to_be_bytes(),
            &self.remove_at.to_be_bytes(),
            &self.author_device,
            &self.author,
        ]
        .concat()
    }

    /// Verify the framing and the signature against the author's device key.
    pub fn verify(self, author: &VerifyingKey, envelope: &[u8]) -> Result<(), EventError> {
        if self.epoch <= 0
            || !(MIN_EVENT_BYTES..=MAX_EVENT_BYTES).contains(&envelope.len())
            || envelope[0] != FORMAT
        {
            return Err(EventError::Encoding);
        }
        if author.as_bytes() != &self.author_device {
            return Err(EventError::Signature);
        }
        let (sealed, signature) = envelope.split_last_chunk().ok_or(EventError::Encoding)?;
        author
            .verify_strict(
                &[self.associated_data().as_slice(), sealed].concat(),
                &Signature::from_bytes(signature),
            )
            .map_err(|_| EventError::Signature)
    }

    /// Encrypt an event and sign it with the matching device identity.
    pub fn seal(
        self,
        key: &Key,
        author: &DeviceIdentity,
        details: &Details,
    ) -> Result<Vec<u8>, EventError> {
        if self.epoch <= 0 {
            return Err(EventError::Encoding);
        }
        if author.certificate.signing_key().as_bytes() != &self.author_device
            || author.certificate.person_id() != &self.author
        {
            return Err(EventError::Signature);
        }
        let plaintext = details.encode()?;
        if details.remove_at()? != self.remove_at {
            return Err(EventError::Invalid);
        }
        let aad = self.associated_data();
        let mut nonce = [0; NONCE];
        random_bytes(&mut nonce).map_err(|_| EventError::Randomness)?;
        let ciphertext = XChaCha20Poly1305::new(&(**key).into())
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| EventError::Authentication)?;
        let mut envelope = [&[FORMAT], nonce.as_slice(), &ciphertext].concat();
        let signature = author
            .signing_key
            .sign(&[aad.as_slice(), &envelope].concat());
        envelope.extend_from_slice(&signature.to_bytes());
        Ok(envelope)
    }

    /// Verify the certified author, decrypt, and check that the plaintext removal time
    /// belongs to the signed end.
    pub fn open(
        self,
        key: &Key,
        author: &DeviceCertificate,
        envelope: &[u8],
    ) -> Result<Details, EventError> {
        if author.person_id() != &self.author {
            return Err(EventError::Signature);
        }
        self.verify(author.signing_key(), envelope)?;
        let nonce: [u8; NONCE] = envelope[1..HEADER]
            .try_into()
            .map_err(|_| EventError::Encoding)?;
        let plaintext = Zeroizing::new(
            XChaCha20Poly1305::new(&(**key).into())
                .decrypt(
                    &XNonce::from(nonce),
                    Payload {
                        msg: &envelope[HEADER..envelope.len() - SIGNATURE],
                        aad: &self.associated_data(),
                    },
                )
                .map_err(|_| EventError::Authentication)?,
        );
        let details = Details::decode(&plaintext)?;
        if details.remove_at()? != self.remove_at {
            return Err(EventError::Invalid);
        }
        Ok(details)
    }
}

/// A stored event and the fields the server sees.
#[derive(Clone, Serialize, Deserialize)]
pub struct EventRecord {
    #[serde(with = "wire_id")]
    pub id: [u8; 32],
    #[serde(with = "wire_id")]
    pub author: [u8; 32],
    #[serde(with = "wire_id")]
    pub author_device: [u8; 32],
    /// The author's public device certificate, which outlives their membership.
    pub certificate: String,
    pub approved: bool,
    pub epoch: i64,
    pub remove_at: i64,
    pub envelope: String,
    /// Who answered; servers from before answers send none.
    #[serde(default)]
    pub answers: Vec<AnswerRecord>,
    /// When the author or a manager last reminded those who haven't answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminded_at: Option<i64>,
}

#[derive(Serialize, Deserialize)]
pub struct NewEvent {
    #[serde(with = "wire_id")]
    pub id: [u8; 32],
    pub epoch: i64,
    pub remove_at: i64,
    pub envelope: String,
}

#[derive(Serialize, Deserialize)]
pub struct Calendar {
    pub enabled: bool,
    pub events: Vec<EventRecord>,
}

#[derive(Serialize, Deserialize)]
pub struct CalendarSetting {
    pub enabled: bool,
}

/// Differs from the event domain, so an answer can't pass as an event or the other way.
const ANSWER_DOMAIN: &[u8] = b"rusttalk-event-answer-v1";
/// Format, nonce, the sealed answer byte with its tag, and the signature. Every answer is
/// this long, so the server can't tell them apart by size.
pub const ANSWER_BYTES: usize = HEADER + 1 + TAG + SIGNATURE;
/// How often an event's author or a manager may remind people, in seconds.
pub const REMIND_EVERY: i64 = 60 * 60;

/// Whether someone comes to an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Going = 1,
    NotGoing = 2,
}

/// Context authenticated by both encryption and the device signature of an answer.
#[derive(Clone, Copy)]
pub struct AnswerContext {
    pub server_id: [u8; 32],
    pub event_id: [u8; 32],
    pub epoch: i64,
    pub device: [u8; 32],
    pub person: [u8; 32],
}

impl AnswerContext {
    fn associated_data(self) -> Vec<u8> {
        [
            ANSWER_DOMAIN,
            &self.server_id,
            &self.event_id,
            &self.epoch.to_be_bytes(),
            &self.device,
            &self.person,
        ]
        .concat()
    }

    /// Verify the framing and the signature against the person's device key.
    pub fn verify(self, device: &VerifyingKey, envelope: &[u8]) -> Result<(), EventError> {
        if self.epoch <= 0 || envelope.len() != ANSWER_BYTES || envelope[0] != FORMAT {
            return Err(EventError::Encoding);
        }
        if device.as_bytes() != &self.device {
            return Err(EventError::Signature);
        }
        let (sealed, signature) = envelope.split_last_chunk().ok_or(EventError::Encoding)?;
        device
            .verify_strict(
                &[self.associated_data().as_slice(), sealed].concat(),
                &Signature::from_bytes(signature),
            )
            .map_err(|_| EventError::Signature)
    }

    pub fn seal(
        self,
        key: &Key,
        person: &DeviceIdentity,
        answer: Answer,
    ) -> Result<Vec<u8>, EventError> {
        if self.epoch <= 0 {
            return Err(EventError::Encoding);
        }
        if person.certificate.signing_key().as_bytes() != &self.device
            || person.certificate.person_id() != &self.person
        {
            return Err(EventError::Signature);
        }
        let aad = self.associated_data();
        let mut nonce = [0; NONCE];
        random_bytes(&mut nonce).map_err(|_| EventError::Randomness)?;
        let ciphertext = XChaCha20Poly1305::new(&(**key).into())
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &[answer as u8],
                    aad: &aad,
                },
            )
            .map_err(|_| EventError::Authentication)?;
        let mut envelope = [&[FORMAT], nonce.as_slice(), &ciphertext].concat();
        let signature = person
            .signing_key
            .sign(&[aad.as_slice(), &envelope].concat());
        envelope.extend_from_slice(&signature.to_bytes());
        Ok(envelope)
    }

    pub fn open(
        self,
        key: &Key,
        person: &DeviceCertificate,
        envelope: &[u8],
    ) -> Result<Answer, EventError> {
        if person.person_id() != &self.person {
            return Err(EventError::Signature);
        }
        self.verify(person.signing_key(), envelope)?;
        let nonce: [u8; NONCE] = envelope[1..HEADER]
            .try_into()
            .map_err(|_| EventError::Encoding)?;
        let plaintext = XChaCha20Poly1305::new(&(**key).into())
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &envelope[HEADER..envelope.len() - SIGNATURE],
                    aad: &self.associated_data(),
                },
            )
            .map_err(|_| EventError::Authentication)?;
        match plaintext.as_slice() {
            [1] => Ok(Answer::Going),
            [2] => Ok(Answer::NotGoing),
            _ => Err(EventError::Encoding),
        }
    }
}

/// A stored answer and the fields the server sees: who answered, not what.
#[derive(Clone, Serialize, Deserialize)]
pub struct AnswerRecord {
    #[serde(with = "wire_id")]
    pub person: [u8; 32],
    #[serde(with = "wire_id")]
    pub device: [u8; 32],
    pub certificate: String,
    pub epoch: i64,
    pub envelope: String,
}

#[derive(Serialize, Deserialize)]
pub struct NewAnswer {
    pub epoch: i64,
    pub envelope: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::encode, keys::random_key};

    fn author() -> DeviceIdentity {
        DeviceIdentity::create("test").expect("test identity").1
    }

    fn details() -> Details {
        Details {
            title: "Filmabend".into(),
            description: "Wir schauen zusammen".into(),
            start: 1_800_000_000,
            end: 1_800_007_200,
            place: Place::Irl("Bei Jakob".into()),
        }
    }

    fn context(author: &DeviceIdentity, details: &Details) -> EventContext {
        EventContext {
            server_id: [1; 32],
            id: [2; 32],
            epoch: 3,
            remove_at: details.end + GRACE,
            author_device: author.certificate.signing_key().to_bytes(),
            author: *author.certificate.person_id(),
        }
    }

    #[test]
    fn opens_only_unchanged_and_in_its_own_context() {
        let (me, other) = (author(), author());
        let (key, details) = (random_key().unwrap(), details());
        let context = context(&me, &details);
        let sealed = context.seal(&key, &me, &details).unwrap();
        assert_eq!(
            context.open(&key, &me.certificate, &sealed).unwrap(),
            details
        );

        let changes: [fn(&mut EventContext); 6] = [
            |c| c.server_id[0] ^= 1,
            |c| c.id[0] ^= 1,
            |c| c.epoch += 1,
            |c| c.remove_at += 1,
            |c| c.author_device[0] ^= 1,
            |c| c.author[0] ^= 1,
        ];
        for change in changes {
            let mut changed = context;
            change(&mut changed);
            assert!(changed.open(&key, &me.certificate, &sealed).is_err());
        }
        for at in 0..sealed.len() {
            let mut changed = sealed.clone();
            changed[at] ^= 1;
            assert!(context.open(&key, &me.certificate, &changed).is_err());
        }
        assert!(context.open(&key, &other.certificate, &sealed).is_err());
        assert!(context.seal(&key, &other, &details).is_err());
        assert!(
            context
                .open(&random_key().unwrap(), &me.certificate, &sealed)
                .is_err()
        );
    }

    #[test]
    fn every_kind_of_place_survives_sealing() {
        let me = author();
        let key = random_key().unwrap();
        for place in [
            Place::Nowhere,
            Place::Irl("Bahnhofstr. 1, Köln".into()),
            Place::Channel([9; 32]),
        ] {
            let details = Details { place, ..details() };
            let context = context(&me, &details);
            let sealed = context.seal(&key, &me, &details).unwrap();
            assert_eq!(
                context.open(&key, &me.certificate, &sealed).unwrap(),
                details
            );
        }
    }

    #[test]
    fn a_signed_removal_time_must_follow_from_the_end() {
        let me = author();
        let (key, details) = (random_key().unwrap(), details());
        let mut context = context(&me, &details);
        context.remove_at += 60;
        assert!(context.seal(&key, &me, &details).is_err());

        // An author with a changed app can sign any pair, so readers check it as well.
        let aad = context.associated_data();
        let nonce = [4; NONCE];
        let ciphertext = XChaCha20Poly1305::new(&(*key).into())
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: &details.encode().unwrap(),
                    aad: &aad,
                },
            )
            .unwrap();
        let mut forged = [&[FORMAT], nonce.as_slice(), &ciphertext].concat();
        let signature = me.signing_key.sign(&[aad.as_slice(), &forged].concat());
        forged.extend_from_slice(&signature.to_bytes());
        assert!(matches!(
            context.open(&key, &me.certificate, &forged),
            Err(EventError::Invalid)
        ));
    }

    #[test]
    fn limits_count_characters_and_the_largest_event_fits_the_body_limit() {
        let me = author();
        let key = random_key().unwrap();
        let largest = Details {
            title: "🎬".repeat(MAX_TITLE),
            description: "🎬".repeat(MAX_DESCRIPTION),
            place: Place::Irl("🎬".repeat(MAX_PLACE)),
            ..details()
        };
        let context = context(&me, &largest);
        let sealed = context.seal(&key, &me, &largest).unwrap();
        assert_eq!(sealed.len(), MAX_EVENT_BYTES);
        assert_eq!(
            context.open(&key, &me.certificate, &sealed).unwrap(),
            largest
        );
        let body = NewEvent {
            id: [255; 32],
            epoch: i64::MAX,
            remove_at: i64::MAX,
            envelope: encode(&sealed),
        };
        assert!(serde_json::to_vec(&body).unwrap().len() <= BODY_LIMIT);

        let invalid = [
            Details {
                title: " ".into(),
                ..details()
            },
            Details {
                title: "x".repeat(MAX_TITLE + 1),
                ..details()
            },
            Details {
                description: "x".repeat(MAX_DESCRIPTION + 1),
                ..details()
            },
            Details {
                place: Place::Irl(" ".into()),
                ..details()
            },
            Details {
                place: Place::Irl("x".repeat(MAX_PLACE + 1)),
                ..details()
            },
            Details {
                end: details().start,
                ..details()
            },
        ];
        for details in invalid {
            assert!(details.encode().is_err());
        }
    }

    #[test]
    fn an_answer_opens_only_for_its_event_person_and_key() {
        let (me, other) = (author(), author());
        let key = random_key().unwrap();
        let context = AnswerContext {
            server_id: [1; 32],
            event_id: [2; 32],
            epoch: 3,
            device: me.certificate.signing_key().to_bytes(),
            person: *me.certificate.person_id(),
        };
        for answer in [Answer::Going, Answer::NotGoing] {
            let sealed = context.seal(&key, &me, answer).unwrap();
            assert_eq!(sealed.len(), ANSWER_BYTES);
            assert_eq!(
                context.open(&key, &me.certificate, &sealed).unwrap(),
                answer
            );
        }
        let sealed = context.seal(&key, &me, Answer::Going).unwrap();
        let changes: [fn(&mut AnswerContext); 5] = [
            |c| c.server_id[0] ^= 1,
            |c| c.event_id[0] ^= 1,
            |c| c.epoch += 1,
            |c| c.device[0] ^= 1,
            |c| c.person[0] ^= 1,
        ];
        for change in changes {
            let mut changed = context;
            change(&mut changed);
            assert!(changed.open(&key, &me.certificate, &sealed).is_err());
        }
        for at in 0..sealed.len() {
            let mut changed = sealed.clone();
            changed[at] ^= 1;
            assert!(context.open(&key, &me.certificate, &changed).is_err());
        }
        assert!(context.open(&key, &other.certificate, &sealed).is_err());
        assert!(context.seal(&key, &other, Answer::Going).is_err());
    }
}
