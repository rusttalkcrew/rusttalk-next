use std::collections::HashSet;

use anyhow::{Result, ensure};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    auth::{decode_bytes, encode},
    identity::{DeviceCertificate, PersonId},
    keys::{self, EnvelopeContext, Kind, Scope, wire_id},
    messages::{MAX_MESSAGE_BYTES, MessageContext, MessageError},
};

use super::KeyClient;
pub use crate::messages::{MAX_TEXT_CHARS, MessageBody, MessageRecord};

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
        let response: CreatedDm = self
            .request(Method::POST, "/v1/dms")
            .json(&json!({
                "id": encode(&scope.id), "peer": encode(peer),
                "commitment": encode(&context.commitment), "envelope": envelope,
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
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
        let scope = Scope {
            kind: Kind::Channel,
            id: *channel,
        };
        let id = *keys::random_key()?;
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
        Ok(record)
    }

    /// Verify a certified sender, decrypt the message, and reject duplicate IDs or unauthorized edits.
    pub async fn open_message(
        &mut self,
        channel: &[u8; 32],
        record: &MessageRecord,
    ) -> Result<MessageBody> {
        ensure!(!self.received.contains(&record.id), MessageError::Replay);
        let body = self.decrypt_record(channel, record).await?;
        if let Some(original) = body.edit_of {
            self.check_edit(channel, &record.id, &record.person, &original)
                .await?;
        }
        remember(&mut self.received, record.id)?;
        Ok(body)
    }

    /// Remove a message the authenticated person owns or can moderate.
    pub async fn delete_message(&self, channel: &[u8; 32], id: &[u8; 32]) -> Result<()> {
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

fn remember(received: &mut HashSet<[u8; 32]>, id: [u8; 32]) -> Result<(), MessageError> {
    if received.insert(id) {
        Ok(())
    } else {
        Err(MessageError::Replay)
    }
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

    #[test]
    fn rejects_replayed_ids_and_edits_by_other_people() {
        let mut received = HashSet::new();
        assert_eq!(remember(&mut received, [1; 32]), Ok(()));
        assert_eq!(remember(&mut received, [1; 32]), Err(MessageError::Replay));
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
