use anyhow::{Result, ensure};
use reqwest::{Method, StatusCode};

use crate::{
    auth::{decode_bytes, encode},
    calendar::{
        Answer, AnswerContext, AnswerRecord, Calendar, Details, EventContext, EventError,
        EventRecord, MAX_EVENT_BYTES, NewAnswer, NewEvent,
    },
    identity::DeviceCertificate,
    keys,
    messages::MessageError,
};

use super::KeyClient;

/// The server refused an event change, with its error code.
#[derive(Debug, thiserror::Error)]
#[error("event change refused ({status}): {code}")]
pub struct EventRefused {
    pub status: StatusCode,
    pub code: String,
}

async fn accepted(reply: reqwest::Response) -> Result<reqwest::Response> {
    let status = reply.status();
    if status.is_success() {
        return Ok(reply);
    }
    let body: serde_json::Value = reply.json().await.unwrap_or_default();
    let code = body["error"].as_str().unwrap_or_default().to_owned();
    Err(EventRefused { status, code }.into())
}

impl KeyClient<'_> {
    /// The server's events this person may see, or `None` on a server from before them.
    pub async fn calendar(&self) -> Result<Option<Calendar>> {
        let reply = self.request(Method::GET, "/v1/calendar").send().await?;
        if reply.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(reply.error_for_status()?.json().await?))
    }

    /// Verify a certified author and decrypt their event.
    pub fn open_event(&self, record: &EventRecord) -> Result<Details> {
        self.revocations
            .check(&record.author, &record.author_device)?;
        ensure!(
            record.envelope.len() <= (MAX_EVENT_BYTES * 4).div_ceil(3),
            EventError::Encoding
        );
        // The root signed the certificate, so it needs no trust in the server that sent it.
        let author =
            DeviceCertificate::verify_public(record.author, &decode_bytes(&record.certificate)?)?;
        let key = self
            .key(self.space(), record.epoch)
            .ok_or(MessageError::MissingKey)?;
        let context = EventContext {
            server_id: self.server.known_server().server_id,
            id: record.id,
            epoch: record.epoch,
            remove_at: record.remove_at,
            author_device: record.author_device,
            author: record.author,
        };
        Ok(context.open(key, &author, &decode_bytes(&record.envelope)?)?)
    }

    /// Suggest an event, or add it at once with "Create events". The server decides which.
    pub async fn add_event(&self, details: &Details) -> Result<EventRecord> {
        let (epoch, key) = self.current_key(self.space())?;
        let cert = &self.identity.certificate;
        let context = EventContext {
            server_id: self.server.known_server().server_id,
            id: *keys::random_key()?,
            epoch,
            remove_at: details.remove_at()?,
            author_device: cert.signing_key().to_bytes(),
            author: *cert.person_id(),
        };
        let new = NewEvent {
            id: context.id,
            epoch,
            remove_at: context.remove_at,
            envelope: encode(&context.seal(key, self.identity, details)?),
        };
        let reply = self
            .request(Method::POST, "/v1/calendar")
            .json(&new)
            .send()
            .await?;
        let record: EventRecord = accepted(reply).await?.json().await?;
        ensure!(
            record.id == new.id && record.envelope == new.envelope,
            EventError::Encoding
        );
        Ok(record)
    }

    /// Approve a suggestion. One this app can't read and verify is never approved.
    pub async fn approve_event(&self, record: &EventRecord) -> Result<()> {
        self.open_event(record)?;
        let reply = self
            .request(Method::POST, &format!("{}/approve", path(&record.id)))
            .send()
            .await?;
        accepted(reply).await?;
        Ok(())
    }

    /// Withdraw, decline or cancel an event.
    pub async fn delete_event(&self, id: &[u8; 32]) -> Result<()> {
        let reply = self.request(Method::DELETE, &path(id)).send().await?;
        accepted(reply).await?;
        Ok(())
    }

    /// Verify who answered and decrypt their answer to `event`.
    pub fn open_answer(&self, event: &[u8; 32], record: &AnswerRecord) -> Result<Answer> {
        self.revocations.check(&record.person, &record.device)?;
        let person =
            DeviceCertificate::verify_public(record.person, &decode_bytes(&record.certificate)?)?;
        let key = self
            .key(self.space(), record.epoch)
            .ok_or(MessageError::MissingKey)?;
        let context = AnswerContext {
            server_id: self.server.known_server().server_id,
            event_id: *event,
            epoch: record.epoch,
            device: record.device,
            person: record.person,
        };
        Ok(context.open(key, &person, &decode_bytes(&record.envelope)?)?)
    }

    /// Say whether this person comes; a later answer replaces the earlier one.
    pub async fn answer_event(&self, event: &[u8; 32], answer: Answer) -> Result<()> {
        let (epoch, key) = self.current_key(self.space())?;
        let cert = &self.identity.certificate;
        let context = AnswerContext {
            server_id: self.server.known_server().server_id,
            event_id: *event,
            epoch,
            device: cert.signing_key().to_bytes(),
            person: *cert.person_id(),
        };
        let body = NewAnswer {
            epoch,
            envelope: encode(&context.seal(key, self.identity, answer)?),
        };
        let reply = self
            .request(Method::PUT, &format!("{}/answer", path(event)))
            .json(&body)
            .send()
            .await?;
        accepted(reply).await?;
        Ok(())
    }

    /// Remind everyone who hasn't answered yet.
    pub async fn remind_event(&self, event: &[u8; 32]) -> Result<()> {
        let reply = self
            .request(Method::POST, &format!("{}/remind", path(event)))
            .send()
            .await?;
        accepted(reply).await?;
        Ok(())
    }
}

fn path(id: &[u8; 32]) -> String {
    format!("/v1/calendar/{}", encode(id))
}
