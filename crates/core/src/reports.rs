//! Reports (#678): a member sends a message they can read to the people who review
//! reports, with evidence those reviewers can check themselves. The evidence is the
//! sender's signed envelope plus the key of that one message (see
//! `MessageContext::disclose`); the message carries the keys of its own files. It is
//! sealed with a fresh report key, wrapped to each reviewer device, so the server stores
//! only ciphertext. See docs/crypto.md, Reports.

use serde::{Deserialize, Serialize};

use crate::{
    identity::PUBLIC_CERT_LEN,
    messages::{MAX_ATTACHMENTS, MAX_MESSAGE_BYTES},
};

/// The optional note, in Unicode scalar values.
pub const MAX_NOTE_CHARS: usize = 500;
/// Reviewer devices one report is wrapped to. A bigger moderation team gets a refusal.
pub const MAX_REVIEWER_DEVICES: usize = 200;
/// An HPKE wrap of the report key: encapsulated key, then the key and its tag.
pub const WRAP_BYTES: usize = 32 + 32 + 16;
/// Stored files one report may name: a message's files and their thumbnails.
pub const MAX_REPORT_FILES: usize = 2 * MAX_ATTACHMENTS;
/// A message and, when it was edited, the edit that was shown.
pub const MAX_EVIDENCE: usize = 2;

const NONCE: usize = 24;
/// SHA-256 commitment to the report key, so every reviewer's wrap must open the same key.
const COMMITMENT: usize = 32;
const TAG: usize = 16;
const SIGNATURE: usize = 64;
/// Channel, message ID, epoch, sender device, sender person, sender certificate,
/// message key and the envelope with its length.
const MAX_ITEM: usize = 32 + 32 + 8 + 32 + 32 + PUBLIC_CERT_LEN + 32 + 2 + MAX_MESSAGE_BYTES;
/// Version, reason, the note with its length, the reporter's certificate, the evidence
/// count and the evidence.
const MAX_PLAINTEXT: usize =
    1 + 1 + 2 + MAX_NOTE_CHARS * 4 + PUBLIC_CERT_LEN + 1 + MAX_EVIDENCE * MAX_ITEM;
/// The sealed report: nonce, key commitment, ciphertext with its tag, the reporter
/// device's signature.
pub const MAX_REPORT_BYTES: usize = NONCE + COMMITMENT + MAX_PLAINTEXT + TAG + SIGNATURE;
/// Base64url length of the largest sealed report, checked before decoding.
pub const MAX_REPORT_TEXT: usize = MAX_REPORT_BYTES.div_ceil(3) * 4;
/// A signed device removal: domain, device key, time and root signature.
const REVOCATION_BYTES: usize = b"rusttalk-device-revocation-v1".len() + 32 + 8 + 64;
/// JSON for the largest report: its body, every wrap, a removal for every reviewer device
/// and the IDs, with room to spare.
pub const BODY_LIMIT: usize = MAX_REPORT_BYTES.div_ceil(3) * 4
    + MAX_REVIEWER_DEVICES * (WRAP_BYTES.div_ceil(3) * 4 + 43 + 32)
    + MAX_REVIEWER_DEVICES * (REVOCATION_BYTES.div_ceil(3) * 4 + 43 + 32)
    + MAX_REPORT_FILES * 48
    + 1024;

const _: () = assert!(MAX_REPORT_BYTES == 30_906);

/// Why someone reports a message. Only reviewers see it: it is inside the sealed report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Spam = 1,
    Harassment = 2,
    Illegal = 3,
    /// Sexual content involving minors.
    ChildSafety = 4,
    Other = 5,
}

impl Reason {
    pub const ALL: [Self; 5] = [
        Self::Spam,
        Self::Harassment,
        Self::Illegal,
        Self::ChildSafety,
        Self::Other,
    ];

    #[cfg(any(feature = "client", test))]
    fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| *reason as u8 == byte)
    }
}

/// `POST /v1/reports`. IDs and keys are base64url.
#[derive(Serialize, Deserialize)]
pub struct NewReport {
    /// Random, chosen by the reporter; the sealed report is bound to it.
    pub id: String,
    pub channel_id: String,
    pub message_id: String,
    /// The edit of the message that was shown, when it was edited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_id: Option<String>,
    /// The stored files the message uses, which reviewers may then download.
    #[serde(default)]
    pub file_ids: Vec<String>,
    pub body: String,
    /// The report key for each reviewer device.
    pub keys: Vec<ReportKey>,
    /// Signed removals of reviewer devices the server still lists but the reporter's app
    /// knows were removed, so it wraps nothing to them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted: Vec<SignedRevocation>,
}

/// A root-signed device removal (`identity::Revocation`), base64url, with its person.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SignedRevocation {
    pub person: String,
    pub record: String,
}

#[derive(Serialize, Deserialize)]
pub struct ReportKey {
    /// The device's Ed25519 key.
    pub device: String,
    pub key: String,
}

/// A reported message as the server stored it when the report came in: its ID, the
/// device that signed it, its epoch and the SHA-256 of its envelope. Reviewers check the
/// evidence against it, so a removed or stolen device can't put new words under the ID of
/// a message its owner really sent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: String,
    pub device: String,
    pub epoch: i64,
    pub hash: String,
}

/// One open report in `GET /v1/reports`.
#[derive(Clone, Serialize, Deserialize)]
pub struct ReportRecord {
    pub id: String,
    pub channel_id: String,
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit_id: Option<String>,
    pub reporter: String,
    pub sender: String,
    /// Reported from a DM: only the owner sees it.
    pub dm: bool,
    pub at: i64,
    pub body: String,
    /// The report key wrapped to the asking device; `None` when that device was not a
    /// reviewer when the report was sent.
    #[serde(default)]
    pub key: Option<String>,
    /// The message, then the shown edit, as stored (see `StoredMessage`).
    #[serde(default)]
    pub stored: Vec<StoredMessage>,
    /// Signed removals of the sender's and reporter's devices the server knows, also of
    /// people no longer members, for the reviewer to check itself.
    #[serde(default)]
    pub revocations: Vec<SignedRevocation>,
}

/// What a reviewer did about a report (`POST /v1/reports/{id}/resolve`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Dismissed,
    /// The server deletes the reported message (and the shown edit).
    Deleted,
    /// Already done through the normal endpoint; the server checks it took effect.
    TimedOut,
    Kicked,
    Banned,
}

#[cfg(any(feature = "client", test))]
pub use sealed::*;

#[cfg(any(feature = "client", test))]
mod sealed {
    use chacha20poly1305::{
        KeyInit, XChaCha20Poly1305, XNonce,
        aead::{Aead, Payload},
    };
    use ed25519_dalek::{Signature, Signer};
    use hpke::{
        Deserializable, Kem, OpModeR, OpModeS, Serializable, aead::ChaCha20Poly1305,
        kdf::HkdfSha256, kem::X25519HkdfSha256, single_shot_open, single_shot_seal_with_rng,
    };
    use rand::{SeedableRng, rngs::StdRng};
    use sha2::{Digest, Sha256};
    use thiserror::Error;
    use zeroize::Zeroizing;

    use super::*;
    use crate::{
        auth::encode,
        identity::{DeviceCertificate, DeviceIdentity, PersonId, random_bytes},
        keys::{Key, random_key},
        messages::{MessageBody, MessageContext, MessageError},
    };

    type Dh = X25519HkdfSha256;
    const VERSION: u8 = 1;
    const DOMAIN: &[u8] = b"rusttalk-report-v1";
    const KEY_DOMAIN: &[u8] = b"rusttalk-report-key-v1";
    const COMMIT_DOMAIN: &[u8] = b"rusttalk-report-commit-v1";

    #[derive(Debug, Error, PartialEq, Eq)]
    pub enum ReportError {
        #[error("invalid report encoding")]
        Encoding,
        #[error("report key or encryption failed")]
        Authentication,
        #[error("invalid reporter certificate or signature")]
        Reporter,
        #[error("reported message failed verification: {0}")]
        Message(#[from] MessageError),
        #[error("the shown edit is not an edit of the reported message by its sender")]
        Edit,
        #[error("the report key doesn't match the reporter's commitment")]
        Commitment,
        #[error("the report doesn't match the message the server stored")]
        Stored,
        #[error("no reviewer devices")]
        NoReviewers,
        #[error("report randomness unavailable")]
        Randomness,
    }

    /// One reported message: its signed envelope, the certificate of the device that
    /// signed it, and the key of only this message.
    pub struct Evidence {
        pub context: MessageContext,
        pub sender: DeviceCertificate,
        pub message_key: Key,
        pub envelope: Vec<u8>,
    }

    /// What a reporter seals: why, an optional note, and the message (then the shown
    /// edit of it, if any).
    pub struct Report {
        pub reason: Reason,
        pub note: String,
        pub evidence: Vec<Evidence>,
    }

    /// A report a reviewer opened and verified.
    pub struct OpenedReport {
        pub reason: Reason,
        pub note: String,
        /// The device that signed the report, certified by the listed reporter.
        pub reporter_device: [u8; 32],
        /// The message, then the shown edit of it, each verified against its sender's
        /// certificate and opened with its own disclosed key.
        pub messages: Vec<(MessageContext, DeviceCertificate, MessageBody)>,
        /// The disclosed message keys, in the same order, to let a reviewer who holds the
        /// channel key confirm them (see `confirms`).
        pub message_keys: Vec<Key>,
        pub envelopes: Vec<Vec<u8>>,
    }

    /// Bound to the server, report, reporter and whether it came from a DM (so a server
    /// can't present a DM report as a channel one, or the other way round).
    fn associated_data(
        server: &[u8; 32],
        report: &[u8; 32],
        reporter: &PersonId,
        dm: bool,
    ) -> Vec<u8> {
        [DOMAIN, server, report, reporter, &[dm as u8]].concat()
    }

    fn commitment(key: &Key) -> [u8; 32] {
        Sha256::new()
            .chain_update(COMMIT_DOMAIN)
            .chain_update(key.as_ref())
            .finalize()
            .into()
    }

    fn key_info(server: &[u8; 32], report: &[u8; 32], device: &[u8; 32]) -> Vec<u8> {
        [KEY_DOMAIN, server, report, device].concat()
    }

    impl Report {
        fn encode(&self, reporter: &DeviceCertificate) -> Result<Zeroizing<Vec<u8>>, ReportError> {
            if self.note.chars().count() > MAX_NOTE_CHARS
                || !(1..=MAX_EVIDENCE).contains(&self.evidence.len())
            {
                return Err(ReportError::Encoding);
            }
            let reporter = reporter.public_bytes().map_err(|_| ReportError::Reporter)?;
            let mut out = Zeroizing::new(Vec::with_capacity(MAX_PLAINTEXT));
            out.extend_from_slice(&[VERSION, self.reason as u8]);
            out.extend_from_slice(&(self.note.len() as u16).to_be_bytes());
            out.extend_from_slice(self.note.as_bytes());
            out.extend_from_slice(reporter);
            out.push(self.evidence.len() as u8);
            for item in &self.evidence {
                let context = &item.context;
                let sender = item
                    .sender
                    .public_bytes()
                    .map_err(|_| ReportError::Encoding)?;
                if item.envelope.len() > MAX_MESSAGE_BYTES {
                    return Err(ReportError::Encoding);
                }
                out.extend_from_slice(&context.channel_id);
                out.extend_from_slice(&context.id);
                out.extend_from_slice(&context.epoch.to_be_bytes());
                out.extend_from_slice(&context.sender_device);
                out.extend_from_slice(&context.person);
                out.extend_from_slice(sender);
                out.extend_from_slice(item.message_key.as_ref());
                out.extend_from_slice(&(item.envelope.len() as u16).to_be_bytes());
                out.extend_from_slice(&item.envelope);
            }
            debug_assert!(out.len() <= MAX_PLAINTEXT);
            Ok(out)
        }

        /// Seal for every reviewer device: the sealed report, and each device's Ed25519 key
        /// with its wrap of the report key. The reporter's device signs the sealed report.
        #[allow(clippy::type_complexity)]
        pub fn seal(
            &self,
            server: &[u8; 32],
            id: &[u8; 32],
            dm: bool,
            reporter: &DeviceIdentity,
            reviewers: &[&DeviceCertificate],
        ) -> Result<(Vec<u8>, Vec<([u8; 32], Vec<u8>)>), ReportError> {
            if reviewers.is_empty() || reviewers.len() > MAX_REVIEWER_DEVICES {
                return Err(ReportError::NoReviewers);
            }
            let plaintext = self.encode(&reporter.certificate)?;
            let key = random_key().map_err(|_| ReportError::Randomness)?;
            let aad = associated_data(server, id, reporter.certificate.person_id(), dm);
            let mut nonce = [0; NONCE];
            random_bytes(&mut nonce).map_err(|_| ReportError::Randomness)?;
            let ciphertext = XChaCha20Poly1305::new(&(*key).into())
                .encrypt(
                    &XNonce::from(nonce),
                    Payload {
                        msg: &plaintext,
                        aad: &aad,
                    },
                )
                .map_err(|_| ReportError::Authentication)?;
            let mut sealed = [nonce.as_slice(), &commitment(&key), &ciphertext].concat();
            let signature = reporter
                .signing_key
                .sign(&[aad.as_slice(), &sealed].concat());
            sealed.extend_from_slice(&signature.to_bytes());
            let wraps = reviewers
                .iter()
                .map(|device| {
                    let id_key = device.signing_key().to_bytes();
                    Ok((id_key, wrap(server, id, device, &key)?))
                })
                .collect::<Result<_, ReportError>>()?;
            Ok((sealed, wraps))
        }
    }

    pub(super) fn wrap(
        server: &[u8; 32],
        report: &[u8; 32],
        device: &DeviceCertificate,
        key: &Key,
    ) -> Result<Vec<u8>, ReportError> {
        let public = <Dh as Kem>::PublicKey::from_bytes(device.agreement_key())
            .map_err(|_| ReportError::Encoding)?;
        let mut rng = StdRng::from_seed(*random_key().map_err(|_| ReportError::Randomness)?);
        let info = key_info(server, report, device.signing_key().as_bytes());
        let (enc, ciphertext) = single_shot_seal_with_rng::<ChaCha20Poly1305, HkdfSha256, Dh>(
            &OpModeS::Base,
            &public,
            &info,
            key.as_ref(),
            &[],
            &mut rng,
        )
        .map_err(|_| ReportError::Authentication)?;
        Ok([enc.to_bytes().as_slice(), ciphertext.as_slice()].concat())
    }

    fn unwrap(
        server: &[u8; 32],
        report: &[u8; 32],
        recipient: &DeviceIdentity,
        bytes: &[u8],
    ) -> Result<Key, ReportError> {
        if bytes.len() != WRAP_BYTES {
            return Err(ReportError::Encoding);
        }
        let device = recipient.certificate.signing_key().as_bytes();
        let secret = <Dh as Kem>::PrivateKey::from_bytes(recipient.agreement_key.as_bytes())
            .map_err(|_| ReportError::Encoding)?;
        let enc = <Dh as Kem>::EncappedKey::from_bytes(&bytes[..32])
            .map_err(|_| ReportError::Encoding)?;
        let key = Zeroizing::new(
            single_shot_open::<ChaCha20Poly1305, HkdfSha256, Dh>(
                &OpModeR::Base,
                &secret,
                &enc,
                &key_info(server, report, device),
                &bytes[32..],
                &[],
            )
            .map_err(|_| ReportError::Authentication)?,
        );
        Ok(Key::new(
            key.as_slice()
                .try_into()
                .map_err(|_| ReportError::Encoding)?,
        ))
    }

    /// Split off `N` bytes, or fail as a bad encoding.
    fn take<'a, const N: usize>(bytes: &mut &'a [u8]) -> Result<&'a [u8; N], ReportError> {
        let (head, rest) = bytes.split_first_chunk().ok_or(ReportError::Encoding)?;
        *bytes = rest;
        Ok(head)
    }

    fn take_slice<'a>(bytes: &mut &'a [u8], length: usize) -> Result<&'a [u8], ReportError> {
        let (head, rest) = bytes
            .split_at_checked(length)
            .ok_or(ReportError::Encoding)?;
        *bytes = rest;
        Ok(head)
    }

    fn take_length(bytes: &mut &[u8]) -> Result<usize, ReportError> {
        Ok(u16::from_be_bytes(*take::<2>(bytes)?) as usize)
    }

    /// Open a report as one of its reviewers: unwrap the report key, decrypt, check the
    /// reporter's certificate and signature, then verify and open every reported message
    /// with its disclosed key. `reporter` and `id` come from the server's listing and are
    /// bound to the sealed report, so the server can't relabel it.
    pub fn open(
        server: &[u8; 32],
        id: &[u8; 32],
        reporter: &PersonId,
        dm: bool,
        recipient: &DeviceIdentity,
        wrap: &[u8],
        sealed: &[u8],
    ) -> Result<OpenedReport, ReportError> {
        if sealed.len() > MAX_REPORT_BYTES || sealed.len() < NONCE + COMMITMENT + TAG + SIGNATURE {
            return Err(ReportError::Encoding);
        }
        let key = unwrap(server, id, recipient, wrap)?;
        let aad = associated_data(server, id, reporter, dm);
        let (signed, signature) = sealed
            .split_last_chunk::<SIGNATURE>()
            .ok_or(ReportError::Encoding)?;
        let (nonce, rest) = signed
            .split_first_chunk::<NONCE>()
            .ok_or(ReportError::Encoding)?;
        let (committed, ciphertext) = rest
            .split_first_chunk::<COMMITMENT>()
            .ok_or(ReportError::Encoding)?;
        // Checked against the signed commitment below: every reviewer gets the same key,
        // so the same report.
        if commitment(&key) != *committed {
            return Err(ReportError::Commitment);
        }
        let plaintext = Zeroizing::new(
            XChaCha20Poly1305::new(&(*key).into())
                .decrypt(
                    &XNonce::from(*nonce),
                    Payload {
                        msg: ciphertext,
                        aad: &aad,
                    },
                )
                .map_err(|_| ReportError::Authentication)?,
        );
        let mut rest = plaintext.as_slice();
        let [version, reason] = *take::<2>(&mut rest)?;
        if version != VERSION {
            return Err(ReportError::Encoding);
        }
        let reason = Reason::from_byte(reason).ok_or(ReportError::Encoding)?;
        let length = take_length(&mut rest)?;
        if length > MAX_NOTE_CHARS * 4 {
            return Err(ReportError::Encoding);
        }
        let note = std::str::from_utf8(take_slice(&mut rest, length)?)
            .map_err(|_| ReportError::Encoding)?;
        if note.chars().count() > MAX_NOTE_CHARS {
            return Err(ReportError::Encoding);
        }
        let certificate = take::<PUBLIC_CERT_LEN>(&mut rest)?;
        let device = DeviceCertificate::verify_public(*reporter, certificate)
            .map_err(|_| ReportError::Reporter)?;
        device
            .signing_key()
            .verify_strict(
                &[aad.as_slice(), signed].concat(),
                &Signature::from_bytes(signature),
            )
            .map_err(|_| ReportError::Reporter)?;
        let [count] = *take::<1>(&mut rest)?;
        if !(1..=MAX_EVIDENCE).contains(&(count as usize)) {
            return Err(ReportError::Encoding);
        }
        let mut opened = OpenedReport {
            reason,
            note: note.to_owned(),
            reporter_device: device.signing_key().to_bytes(),
            messages: Vec::new(),
            message_keys: Vec::new(),
            envelopes: Vec::new(),
        };
        for _ in 0..count {
            let context = MessageContext {
                server_id: *server,
                channel_id: *take(&mut rest)?,
                id: *take(&mut rest)?,
                epoch: i64::from_be_bytes(*take(&mut rest)?),
                sender_device: *take(&mut rest)?,
                person: *take(&mut rest)?,
            };
            let sender = DeviceCertificate::verify_public(
                context.person,
                take::<PUBLIC_CERT_LEN>(&mut rest)?,
            )
            .map_err(|_| MessageError::Signature)?;
            let message_key = Key::new(*take(&mut rest)?);
            let length = take_length(&mut rest)?;
            if length > MAX_MESSAGE_BYTES {
                return Err(ReportError::Encoding);
            }
            let envelope = take_slice(&mut rest, length)?;
            let body = context.open_disclosed(&message_key, &sender, envelope)?;
            opened.messages.push((context, sender, body));
            opened.message_keys.push(message_key);
            opened.envelopes.push(envelope.to_vec());
        }
        if !rest.is_empty() {
            return Err(ReportError::Encoding);
        }
        // The first item is the message itself, never an edit; the second is the edit that
        // was shown: another record that edits the first, in the same channel, by the same
        // person.
        let [(original, _, first), rest @ ..] = opened.messages.as_slice() else {
            return Err(ReportError::Encoding);
        };
        if first.edit_of.is_some() {
            return Err(ReportError::Edit);
        }
        if let [(edit, _, body)] = rest
            && (edit.id == original.id
                || edit.channel_id != original.channel_id
                || edit.person != original.person
                || body.edit_of != Some(original.id))
        {
            return Err(ReportError::Edit);
        }
        Ok(opened)
    }

    /// Whether a reviewer who holds the channel key derived the disclosed keys.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Confirmation {
        /// This reviewer doesn't hold every epoch key involved.
        Absent,
        Matched,
        /// The reporter disclosed a key other readers don't have: the content may differ
        /// from what members saw. Treat the report as unverified.
        Mismatch,
    }

    impl OpenedReport {
        /// For a reviewer who also holds the channel's epoch key: whether each disclosed
        /// key is the one every reader derives. A sender colluding with the reporter could
        /// otherwise craft a message that opens differently under two keys.
        pub fn confirms(&self, key_for: impl Fn(&MessageContext) -> Option<Key>) -> Confirmation {
            let mut all = true;
            for (((context, sender, _), disclosed), envelope) in self
                .messages
                .iter()
                .zip(&self.message_keys)
                .zip(&self.envelopes)
            {
                let Some(key) = key_for(context) else {
                    all = false;
                    continue;
                };
                let derived = context.disclose(&key, sender, envelope);
                if !derived.is_ok_and(|derived| *derived == **disclosed) {
                    return Confirmation::Mismatch;
                }
            }
            match all {
                true => Confirmation::Matched,
                false => Confirmation::Absent,
            }
        }

        /// Check the report against the server's listing: the channel, message, shown edit
        /// and sender it names, and each message as the server stored it when the report
        /// came in (signing device, epoch and envelope hash). Anything else would be
        /// evidence the sender's account never posted under that ID.
        pub fn check_listing(&self, record: &ReportRecord) -> Result<(), ReportError> {
            let [(original, ..), rest @ ..] = self.messages.as_slice() else {
                return Err(ReportError::Encoding);
            };
            let labels = encode(&original.channel_id) == record.channel_id
                && encode(&original.id) == record.message_id
                && encode(&original.person) == record.sender
                && rest.first().map(|(edit, ..)| encode(&edit.id)) == record.edit_id;
            if !labels || record.stored.len() != self.messages.len() {
                return Err(ReportError::Stored);
            }
            for (((context, ..), envelope), stored) in self
                .messages
                .iter()
                .zip(&self.envelopes)
                .zip(&record.stored)
            {
                let hash: [u8; 32] = Sha256::digest(envelope).into();
                if *stored
                    != (StoredMessage {
                        id: encode(&context.id),
                        device: encode(&context.sender_device),
                        epoch: context.epoch,
                        hash: encode(&hash),
                    })
                {
                    return Err(ReportError::Stored);
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
