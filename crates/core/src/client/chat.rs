use std::collections::{HashMap, VecDeque};

use anyhow::{Result, ensure};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    auth::{decode_bytes, encode},
    identity::{DeviceCertificate, PersonId},
    keys::{self, EnvelopeContext, Kind, Scope, wire_id},
    messages::{MAX_MESSAGE_BYTES, MessageContext, MessageError},
};

use super::{KeyClient, keys::DmPin};
pub use crate::messages::{MAX_TEXT_CHARS, MessageBody, MessageRecord};

// shortcut: replay protection forgets the oldest IDs past this cap, same as server retention; persist with W14 sync.
const OPENED_PER_CHANNEL: usize = 10_000;

/// Opened records of one channel, oldest first, and the edits seen for each original.
#[derive(Default)]
pub(super) struct Opened {
    order: VecDeque<[u8; 32]>,
    bodies: HashMap<[u8; 32], ([u8; 32], MessageBody)>,
    edits: HashMap<[u8; 32], Vec<[u8; 32]>>,
}

impl Opened {
    /// Return the cached body for the same record, or reject a different record under a seen ID.
    fn get(&self, record: &MessageRecord) -> Result<Option<MessageBody>, MessageError> {
        match self.bodies.get(&record.id) {
            Some((seen, body)) if *seen == digest(record) => Ok(Some(body.clone())),
            Some(_) => Err(MessageError::Replay),
            None => Ok(None),
        }
    }

    fn insert(&mut self, record: &MessageRecord, body: &MessageBody) {
        if self.bodies.contains_key(&record.id) {
            return;
        }
        self.bodies
            .insert(record.id, (digest(record), body.clone()));
        self.order.push_back(record.id);
        if let Some(original) = body.edit_of {
            self.edits.entry(original).or_default().push(record.id);
        }
        if self.order.len() > OPENED_PER_CHANNEL
            && let Some(old) = self.order.pop_front()
            && let Some((_, body)) = self.bodies.remove(&old)
            && let Some(original) = body.edit_of
            && let Some(ids) = self.edits.get_mut(&original)
        {
            ids.retain(|id| *id != old);
            if ids.is_empty() {
                self.edits.remove(&original);
            }
        }
    }
}

fn digest(record: &MessageRecord) -> [u8; 32] {
    Sha256::new()
        .chain_update(record.epoch.to_be_bytes())
        .chain_update(record.person)
        .chain_update(record.sender_device)
        .chain_update(record.envelope.as_bytes())
        .finalize()
        .into()
}

/// A readable channel and its encrypted metadata.
#[derive(Deserialize)]
pub struct Channel {
    #[serde(with = "wire_id")]
    pub id: [u8; 32],
    pub kind: String,
    pub category_id: Option<String>,
    pub position: i64,
    pub name_ct: Option<String>,
    pub topic_ct: Option<String>,
    pub peer: Option<String>,
    pub flags: Vec<RoleFlags>,
}

/// One role's channel permissions, using read, write, speak and video bits.
#[derive(Deserialize)]
pub struct RoleFlags {
    pub role_id: String,
    pub flags: u8,
}

/// A visible category and its encrypted name.
#[derive(Deserialize)]
pub struct Category {
    #[serde(with = "wire_id")]
    pub id: [u8; 32],
    pub position: i64,
    pub name_ct: String,
}

/// Readable channels, including DMs, and their visible categories.
#[derive(Deserialize)]
pub struct Catalog {
    pub channels: Vec<Channel>,
    pub categories: Vec<Category>,
}

#[derive(Deserialize)]
struct CreatedDm {
    #[serde(with = "wire_id")]
    id: [u8; 32],
}

#[derive(Serialize)]
struct Page {
    before: Option<String>,
    limit: u32,
}

impl KeyClient<'_> {
    /// Fetch readable channels and categories, with names still encrypted.
    pub async fn catalog(&self) -> Result<Catalog> {
        Ok(self
            .request(Method::GET, "/v1/catalog")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Open or create a DM with a current member and reconcile its key envelopes.
    pub async fn create_dm(&mut self, peer: &PersonId) -> Result<Scope> {
        let scope = Scope {
            kind: Kind::Channel,
            id: *keys::random_key()?,
        };
        let key = keys::random_key()?;
        let context = EnvelopeContext::new(self.server.known_server().server_id, scope, 1, &key);
        let envelope = self.wrap_for_self(context, &key)?;
        // Pinned before anything is wrapped: this key is new, so epoch 1 holds for the pair.
        self.dms.insert(
            scope.id,
            DmPin::With {
                peer: *peer,
                from: Some(1),
            },
        );
        let request = self.request(Method::POST, "/v1/dms").json(&json!({
            "id": encode(&scope.id), "peer": encode(peer),
            "commitment": encode(&context.commitment), "envelope": envelope,
        }));
        let response: Result<CreatedDm> =
            async { Ok(request.send().await?.error_for_status()?.json().await?) }.await;
        // The server had one already, or refused: that one isn't new, so it is pinned
        // as usual and re-keyed before use.
        if response
            .as_ref()
            .is_ok_and(|created| created.id != scope.id)
            || response.is_err()
        {
            self.dms.remove(&scope.id);
        }
        let response = response?;
        self.bind_dm(response.id, *peer);
        self.sync().await?;
        Ok(Scope {
            id: response.id,
            ..scope
        })
    }

    /// Fetch a ciphertext page newest first, optionally before a retained message ID.
    pub async fn message_page(
        &self,
        channel: &[u8; 32],
        before: Option<&[u8; 32]>,
        limit: u32,
    ) -> Result<Vec<MessageRecord>> {
        Ok(self
            .request(Method::GET, &message_path(channel))
            .query(&Page {
                before: before.map(|id| encode(id)),
                limit,
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Fetch a single ciphertext record, for example after a message-created event.
    pub async fn message(&self, channel: &[u8; 32], id: &[u8; 32]) -> Result<MessageRecord> {
        let record: MessageRecord = self
            .request(Method::GET, &record_path(channel, id))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(record.id == *id, MessageError::Encoding);
        Ok(record)
    }

    /// Encrypt, sign and post a message in the current channel epoch.
    pub async fn send_message(
        &mut self,
        channel: &[u8; 32],
        body: &MessageBody,
    ) -> Result<MessageRecord> {
        self.post_message(channel, *keys::random_key()?, body).await
    }

    /// Like `send_message`, with a chosen random ID: a DM posts the same ID to every
    /// server both people share, so readers can merge the copies.
    pub async fn post_message(
        &mut self,
        channel: &[u8; 32],
        id: [u8; 32],
        body: &MessageBody,
    ) -> Result<MessageRecord> {
        let scope = Scope {
            kind: Kind::Channel,
            id: *channel,
        };
        let cert = &self.identity.certificate;
        if let Some(original) = body.edit_of {
            self.check_edit(channel, &id, cert.person_id(), &original)
                .await?;
        }
        let (epoch, key) = self.current_key(scope)?;
        let context = MessageContext {
            server_id: self.server.known_server().server_id,
            channel_id: *channel,
            epoch,
            id,
            sender_device: cert.signing_key().to_bytes(),
            person: *cert.person_id(),
        };
        let envelope = encode(&context.seal(key, self.identity, body)?);
        let record: MessageRecord = self
            .request(Method::POST, &message_path(channel))
            .json(&json!({ "id": encode(&id), "epoch": epoch, "envelope": envelope }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            record.id == id
                && record.epoch == epoch
                && record.sender_device == context.sender_device
                && record.person == context.person
                && record.envelope == envelope,
            MessageError::Encoding
        );
        self.opened
            .entry(*channel)
            .or_default()
            .insert(&record, body);
        Ok(record)
    }

    /// Verify a certified sender, decrypt the message, and reject reused IDs or unauthorized edits.
    /// Opening the same record again returns the cached body.
    pub async fn open_message(
        &mut self,
        channel: &[u8; 32],
        record: &MessageRecord,
    ) -> Result<MessageBody> {
        if let Some(opened) = self.opened.get(channel)
            && let Some(body) = opened.get(record)?
        {
            return Ok(body);
        }
        let body = self.decrypt_record(channel, record).await?;
        if let Some(original) = body.edit_of {
            self.check_edit(channel, &record.id, &record.person, &original)
                .await?;
        }
        self.opened
            .entry(*channel)
            .or_default()
            .insert(record, &body);
        Ok(body)
    }

    /// Remove a message the authenticated person owns or can moderate, with the edits of it this client has opened or sent.
    pub async fn delete_message(&self, channel: &[u8; 32], id: &[u8; 32]) -> Result<()> {
        let edits = self
            .opened
            .get(channel)
            .and_then(|opened| opened.edits.get(id))
            .cloned()
            .unwrap_or_default();
        for edit in edits {
            let reply = self
                .request(Method::DELETE, &record_path(channel, &edit))
                .send()
                .await?;
            // Another client or retention may already have removed the edit.
            if reply.status() != StatusCode::NOT_FOUND {
                reply.error_for_status()?;
            }
        }
        self.request(Method::DELETE, &record_path(channel, id))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn decrypt_record(
        &mut self,
        channel: &[u8; 32],
        record: &MessageRecord,
    ) -> Result<MessageBody> {
        ensure!(
            record.envelope.len() <= (MAX_MESSAGE_BYTES * 4).div_ceil(3),
            MessageError::Encoding
        );
        let bytes = decode_bytes(&record.envelope)?;
        ensure!(bytes.len() == record.size, MessageError::Encoding);
        let sender = self.message_sender(channel, record).await?;
        let scope = Scope {
            kind: Kind::Channel,
            id: *channel,
        };
        let key = self
            .key(scope, record.epoch)
            .ok_or(MessageError::MissingKey)?;
        let context = MessageContext {
            server_id: self.server.known_server().server_id,
            channel_id: *channel,
            epoch: record.epoch,
            id: record.id,
            sender_device: record.sender_device,
            person: record.person,
        };
        Ok(context.open(key, &sender, &bytes)?)
    }

    async fn message_sender(
        &mut self,
        channel: &[u8; 32],
        record: &MessageRecord,
    ) -> Result<DeviceCertificate> {
        if let Some(sender) = self.devices.get(&encode(&record.sender_device))
            && sender.person_id() == &record.person
        {
            return Ok(sender.clone());
        }
        let slot = (*channel, record.sender_device, record.person);
        if let Some(sender) = self.historical_devices.get(&slot) {
            return Ok(sender.clone());
        }
        let path = format!(
            "/v1/channels/{}/message-devices/{}/{}",
            encode(channel),
            encode(&record.sender_device),
            encode(&record.person)
        );
        let certificate: String = self
            .request(Method::GET, &path)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let sender = DeviceCertificate::verify_public(record.person, &decode_bytes(&certificate)?)?;
        ensure!(
            sender.signing_key().as_bytes() == &record.sender_device,
            MessageError::Signature
        );
        self.historical_devices.insert(slot, sender.clone());
        Ok(sender)
    }

    async fn check_edit(
        &mut self,
        channel: &[u8; 32],
        id: &[u8; 32],
        author: &PersonId,
        original: &[u8; 32],
    ) -> Result<()> {
        ensure!(id != original, MessageError::EditAuthor);
        let record = self.message(channel, original).await?;
        let body = self.decrypt_record(channel, &record).await?;
        authorize_edit(author, &record.person, &body)?;
        Ok(())
    }
}

fn message_path(channel: &[u8; 32]) -> String {
    format!("/v1/channels/{}/messages", encode(channel))
}

fn record_path(channel: &[u8; 32], id: &[u8; 32]) -> String {
    format!("{}/{}", message_path(channel), encode(id))
}

fn authorize_edit(
    author: &PersonId,
    original_author: &PersonId,
    original: &MessageBody,
) -> Result<(), MessageError> {
    if author == original_author && original.edit_of.is_none() {
        Ok(())
    } else {
        Err(MessageError::EditAuthor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u8, envelope: &str) -> MessageRecord {
        MessageRecord {
            id: [id; 32],
            epoch: 1,
            person: [2; 32],
            sender_device: [3; 32],
            at: 0,
            size: 0,
            envelope: envelope.into(),
        }
    }

    #[test]
    fn reopens_the_same_record_and_rejects_another_under_its_id() {
        let mut opened = Opened::default();
        let first = record(1, "first");
        let body = MessageBody {
            text: "hello".into(),
            ..Default::default()
        };
        assert!(opened.get(&first).unwrap().is_none());
        opened.insert(&first, &body);
        assert!(opened.get(&first).unwrap() == Some(body));
        assert_eq!(
            opened.get(&record(1, "other")).err(),
            Some(MessageError::Replay)
        );
    }

    #[test]
    fn tracks_edits_until_they_are_evicted() {
        let mut opened = Opened::default();
        let edit = MessageBody {
            edit_of: Some([0; 32]),
            ..Default::default()
        };
        opened.insert(&record(1, "edit"), &edit);
        assert_eq!(opened.edits[&[0; 32]], [[1; 32]]);
        for i in 0..OPENED_PER_CHANNEL {
            let mut filler = record(2, "filler");
            filler.id[..8].copy_from_slice(&(i as u64).to_be_bytes());
            opened.insert(&filler, &MessageBody::default());
        }
        assert!(opened.edits.is_empty() && opened.bodies.len() == OPENED_PER_CHANNEL);
    }

    #[test]
    fn rejects_edits_by_other_people() {
        let original = MessageBody::default();
        assert_eq!(authorize_edit(&[2; 32], &[2; 32], &original), Ok(()));
        assert_eq!(
            authorize_edit(&[1; 32], &[2; 32], &original),
            Err(MessageError::EditAuthor)
        );
        let edit = MessageBody {
            edit_of: Some([0; 32]),
            ..original
        };
        assert_eq!(
            authorize_edit(&[2; 32], &[2; 32], &edit),
            Err(MessageError::EditAuthor)
        );
    }
}
