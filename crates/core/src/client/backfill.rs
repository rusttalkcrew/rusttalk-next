//! A bounded inventory of DM history. Only the author can seal a missing copy.
use std::collections::HashSet;

use anyhow::{Result, ensure};
use reqwest::StatusCode;

use super::{KeyClient, MessageBody};

type Id = [u8; 32];
pub const DM_PAGE: u32 = 32;
pub const DM_DEPTH: usize = 256;

/// Inventories are newest first. Restore originals before later replies and edits.
pub fn missing_dm_ids(source: &[(Id, Id)], target: &[(Id, Id)], me: Id) -> Vec<Id> {
    let mut seen: HashSet<_> = target.iter().map(|(id, _)| *id).collect();
    source
        .iter()
        .rev()
        .filter_map(|&(id, person)| (person == me && seen.insert(id)).then_some(id))
        .collect()
}

struct StoredFile {
    bytes: Vec<u8>,
    thumbnail: Option<Vec<u8>>,
}

pub struct DmHistoryItem {
    id: Id,
    peer: Id,
    author: Id,
    at: i64,
    pub body: MessageBody,
    files: Vec<StoredFile>,
}

impl KeyClient<'_> {
    pub async fn read_dm_copy(
        &mut self,
        channel: &Id,
        peer: Id,
        id: Id,
    ) -> Result<Option<DmHistoryItem>> {
        ensure!(self.dm_private(channel, &peer), "untrusted source DM");
        let record = self.message(channel, &id).await?;
        ensure!(
            record.person == *self.identity.certificate.person_id(),
            "not own DM record"
        );
        if !self.dm_history_epoch(*channel, record.epoch) {
            return Ok(None);
        }
        let mut body = self.open_message(channel, &record).await?;
        // Epoch links and ephemeral transfer offers belong to the source channel.
        if body.key_link.is_some() || body.live.is_some() {
            return Ok(None);
        }
        let mut files = Vec::new();
        for attachment in &mut body.attachments {
            let bytes = self.download_file(&attachment.file, None).await?;
            let thumbnail = match &attachment.thumbnail {
                Some(reference) => Some(self.download_file(reference, None).await?),
                None => None,
            };
            attachment.file = super::files::blob_ref(bytes.len())?;
            attachment.thumbnail = thumbnail
                .as_ref()
                .map(|png| super::files::blob_ref(png.len()))
                .transpose()?;
            files.push(StoredFile { bytes, thumbnail });
        }
        Ok(Some(DmHistoryItem {
            id,
            peer,
            author: record.person,
            at: record.at,
            body,
            files,
        }))
    }

    pub async fn restore_dm_copy(&mut self, channel: &Id, item: DmHistoryItem) -> Result<()> {
        ensure!(
            item.author == *self.identity.certificate.person_id(),
            "not own DM history"
        );
        ensure!(
            self.dm_private(channel, &item.peer),
            "untrusted destination DM"
        );
        // A copy can fall outside our inventory or arrive while the scan is running.
        match self.message(channel, &item.id).await {
            Ok(_) => return Ok(()),
            Err(error)
                if error
                    .downcast_ref::<reqwest::Error>()
                    .and_then(reqwest::Error::status)
                    == Some(StatusCode::NOT_FOUND) => {}
            Err(error) => return Err(error),
        }
        let mut uploaded = 0;
        let result = async {
            for (attachment, file) in item.body.attachments.iter().zip(&item.files) {
                self.upload_prepared(
                    channel,
                    attachment,
                    &file.bytes,
                    file.thumbnail.as_deref(),
                    None,
                )
                .await?;
                uploaded += 1;
            }
            self.post_message_dated(channel, item.id, &item.body, Some(item.at))
                .await
        }
        .await;
        if let Err(error) = result {
            // On a lost response the post may have committed. Keep the files in
            // that case; orphaned uploads have the ordinary server expiry.
            if error
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status)
                .is_some()
                || uploaded < item.body.attachments.len()
            {
                for file in item.body.attachments.into_iter().take(uploaded) {
                    for blob in std::iter::once(file.file).chain(file.thumbnail) {
                        if self.delete_file(&blob.id).await.is_err() {
                            tracing::warn!("could not remove unsent DM backfill file");
                        }
                    }
                }
            }
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_is_unique_own_and_oldest_first() {
        let me = [1; 32];
        let peer = [2; 32];
        let source = [
            ([4; 32], me),
            ([3; 32], me),
            ([2; 32], peer),
            ([1; 32], me),
            ([1; 32], me),
        ];
        assert_eq!(
            missing_dm_ids(&source, &[([3; 32], me)], me),
            [[1; 32], [4; 32]]
        );
    }
}
