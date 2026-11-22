use anyhow::{Result, ensure};
use reqwest::{Method, StatusCode};

use crate::{
    auth::{decode_bytes, encode},
    keys::{Kind, Scope},
    messages::MessageError,
    pages::{Layout, MAX_PAGE_BYTES, PageContext, PageRecord, SavePage},
};

use super::KeyClient;

impl KeyClient<'_> {
    /// Fetch a page channel's document, or `None` before its first save.
    pub async fn page(&self, channel: &[u8; 32]) -> Result<Option<PageRecord>> {
        let reply = self
            .request(Method::GET, &path(channel))
            .send()
            .await?
            .error_for_status()?;
        if reply.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        Ok(Some(reply.json().await?))
    }

    /// Verify a certified editor and decrypt their page.
    pub async fn open_page(&mut self, channel: &[u8; 32], record: &PageRecord) -> Result<Layout> {
        self.revocations
            .check(&record.person, &record.sender_device)?;
        ensure!(
            record.envelope.len() <= (MAX_PAGE_BYTES * 4).div_ceil(3),
            MessageError::Encoding
        );
        let envelope = decode_bytes(&record.envelope)?;
        let sender = self
            .certified_sender(channel, &record.person, &record.sender_device)
            .await?;
        let scope = Scope {
            kind: Kind::Channel,
            id: *channel,
        };
        let key = self
            .key(scope, record.epoch)
            .ok_or(MessageError::MissingKey)?;
        let context = PageContext {
            server_id: self.server.known_server().server_id,
            channel_id: *channel,
            epoch: record.epoch,
            version: record.version,
            sender_device: record.sender_device,
            person: record.person,
        };
        // Fetching the certificate may have taken a while.
        self.revocations
            .check(&record.person, &record.sender_device)?;
        Ok(context.open(key, &sender, &envelope)?)
    }

    /// Replace version `base` with `version`. `None` when someone changed the page meanwhile.
    pub async fn save_page(
        &self,
        channel: &[u8; 32],
        base: i64,
        version: i64,
        layout: &Layout,
    ) -> Result<Option<PageRecord>> {
        let (epoch, key) = self.current_key(Scope {
            kind: Kind::Channel,
            id: *channel,
        })?;
        let cert = &self.identity.certificate;
        let context = PageContext {
            server_id: self.server.known_server().server_id,
            channel_id: *channel,
            epoch,
            version,
            sender_device: cert.signing_key().to_bytes(),
            person: *cert.person_id(),
        };
        let save = SavePage {
            base,
            version,
            epoch,
            envelope: encode(&context.seal(key, self.identity, layout)?),
            keep: layout.kept().iter().map(|id| encode(id)).collect(),
        };
        let reply = self
            .request(Method::PUT, &path(channel))
            .json(&save)
            .send()
            .await?;
        if let Err(error) = reply.error_for_status_ref() {
            let status = reply.status();
            let body = reply.json::<serde_json::Value>().await.unwrap_or_default();
            if status == StatusCode::CONFLICT && body["error"] == "page_changed" {
                return Ok(None);
            }
            return Err(error.into());
        }
        let record: PageRecord = reply.json().await?;
        ensure!(
            record.version == version
                && record.epoch == epoch
                && record.sender_device == context.sender_device
                && record.person == context.person
                && record.envelope == save.envelope,
            MessageError::Encoding
        );
        Ok(Some(record))
    }
}

fn path(channel: &[u8; 32]) -> String {
    format!("/v1/channels/{}/page", encode(channel))
}
