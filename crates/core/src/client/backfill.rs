//! A bounded inventory of DM history. Only the author can seal a missing copy.
use std::collections::HashSet;

use anyhow::{Result, ensure};
use reqwest::StatusCode;

use super::{KeyClient, MessageBody};

type Id = [u8; 32];
pub const DM_PAGE: u32 = 32;
pub const DM_DEPTH: usize = 256;

/// ID, author and timestamp.
pub type InventoryEntry = (Id, Id, i64);

/// Inventories are newest first. Restore originals before later replies and edits.
pub fn missing_dm_ids(source: &[InventoryEntry], target: &[InventoryEntry], me: Id) -> Vec<Id> {
    let mut seen: HashSet<_> = target.iter().map(|&(id, _, _)| id).collect();
    // IDs are random. Use source order when the boundary is shared, otherwise
    // conservatively require a timestamp strictly newer than the boundary.
    let mut candidates = source;
    let mut after = None;
    if target.len() >= DM_DEPTH
        && let Some(&(id, person, at)) = target.last()
    {
        if let Some(end) = source.iter().position(|m| (m.0, m.1) == (id, person)) {
            candidates = &source[..end];
        } else {
            after = Some(at);
        }
    }
    candidates
        .iter()
        .rev()
        .filter_map(|&(id, person, at)| {
            (person == me && after.is_none_or(|boundary| at > boundary) && seen.insert(id))
                .then_some(id)
        })
        .collect()
}

/// Permanent historical POST refusals. A reserved ID only rules out one record.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("DM history record refused")]
    Record,
    #[error("destination cannot accept DM history")]
    Destination,
}

impl Refusal {
    pub(super) fn from_response(status: StatusCode, code: Option<&str>) -> Option<Self> {
        match (status, code) {
            (StatusCode::FORBIDDEN | StatusCode::PAYLOAD_TOO_LARGE, _)
            | (StatusCode::CONFLICT, Some("history_full")) => Some(Self::Destination),
            (StatusCode::CONFLICT, _) => Some(Self::Record),
            _ => None,
        }
    }
}

pub struct DmHistoryItem {
    id: Id,
    peer: Id,
    author: Id,
    at: i64,
    pub body: MessageBody,
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
        if !self
            .dm_history_epoch(*channel, record.epoch)
            .ok_or(super::MessageError::MissingKey)?
        {
            return Ok(None);
        }
        let body = self.open_message(channel, &record).await?;
        // Epoch links and ephemeral transfer offers belong to the source channel.
        if body.key_link.is_some() || body.live.is_some() || !body.attachments.is_empty() {
            return Ok(None);
        }
        Ok(Some(DmHistoryItem {
            id,
            peer,
            author: record.person,
            at: record.at,
            body,
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
        self.post_message_dated(
            channel,
            item.id,
            &item.body,
            super::chat::Posting::Backfill(item.at),
        )
        .await?;
        Ok(())
    }

    pub async fn needs_dm_copy(&self, channel: &Id, peer: Id, id: Id) -> Result<bool> {
        ensure!(self.dm_private(channel, &peer), "untrusted destination DM");
        match self.message(channel, &id).await {
            Ok(_) => Ok(false),
            Err(error)
                if error
                    .downcast_ref::<reqwest::Error>()
                    .and_then(reqwest::Error::status)
                    == Some(StatusCode::NOT_FOUND) =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classify_historical_post_refusals() {
        assert_eq!(
            Refusal::from_response(StatusCode::CONFLICT, Some("message_exists")),
            Some(Refusal::Record)
        );
        assert_eq!(
            Refusal::from_response(StatusCode::CONFLICT, Some("history_full")),
            Some(Refusal::Destination)
        );
        assert_eq!(
            Refusal::from_response(StatusCode::PAYLOAD_TOO_LARGE, Some("file_member_quota")),
            Some(Refusal::Destination)
        );
        assert_eq!(
            Refusal::from_response(StatusCode::TOO_MANY_REQUESTS, None),
            None
        );
    }

    #[test]
    fn capped_inventory_stops_at_destination_boundary() {
        let me = [1; 32];
        let source = [([3; 32], me, 8), ([2; 32], me, 8), ([1; 32], me, 7)];
        let mut target = vec![([9; 32], me, 10); DM_DEPTH];
        *target.last_mut().unwrap() = source[1];
        assert_eq!(missing_dm_ids(&source, &target, me), [[3; 32]]);
        // No common boundary ID: timestamps are the only cross-server ordering.
        *target.last_mut().unwrap() = ([8; 32], me, 8);
        assert!(missing_dm_ids(&source, &target, me).is_empty());
        target.pop();
        assert_eq!(
            missing_dm_ids(&source, &target, me),
            [[1; 32], [2; 32], [3; 32]]
        );
    }

    #[test]
    fn missing_is_unique_own_and_oldest_first() {
        let me = [1; 32];
        let peer = [2; 32];
        let source = [
            ([4; 32], me, 4),
            ([3; 32], me, 3),
            ([2; 32], peer, 2),
            ([1; 32], me, 1),
            ([1; 32], me, 1),
        ];
        assert_eq!(
            missing_dm_ids(&source, &[([3; 32], me, 3)], me),
            [[1; 32], [4; 32]]
        );
    }
}
