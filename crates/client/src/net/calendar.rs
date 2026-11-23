//! The server's events: loaded with the directory, changed from the Events page.

use anyhow::Result;
use reqwest::StatusCode;
use rusttalk_core::{
    calendar::{Details, EventRecord},
    client::{EventRefused, KeyClient},
};
use url::Url;

use super::{Ctx, Update};
use crate::server::{Calendar, Event, Id};

pub enum Change {
    Add(Details),
    /// The event as it was shown, so what gets approved is what was read.
    Approve(Box<EventRecord>),
    Delete(Id),
}

/// Fetch and open the events, or `None` on a server from before them.
pub(super) async fn load(client: &KeyClient<'_>) -> Result<Option<Calendar>> {
    let Some(calendar) = client.calendar().await? else {
        return Ok(None);
    };
    let events = calendar
        .events
        .into_iter()
        .map(|record| {
            let mut event = Event {
                record,
                details: None,
                waiting_for_key: false,
            };
            open(&mut event, |record| client.open_event(record));
            event
        })
        .collect();
    Ok(Some(Calendar {
        enabled: calendar.enabled,
        events,
    }))
}

fn open(event: &mut Event, verify: impl FnOnce(&EventRecord) -> Result<Details>) {
    let result = verify(&event.record);
    event.waiting_for_key = matches!(
        result.as_ref().err().and_then(|e| e.downcast_ref()),
        Some(rusttalk_core::messages::MessageError::MissingKey)
    );
    event.details = result
        .inspect_err(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "event could not be opened");
        })
        .ok();
}

/// Retry only records whose missing epoch has arrived. Verification still runs in full.
pub(super) fn retry(
    calendar: &mut Calendar,
    has_key: impl Fn(i64) -> bool,
    verify: impl Fn(&EventRecord) -> Result<Details>,
) -> bool {
    let mut changed = false;
    for event in &mut calendar.events {
        if event.waiting_for_key && has_key(event.record.epoch) {
            open(event, &verify);
            changed |= event.details.is_some();
        }
    }
    changed
}

/// Apply a change and report back, so the form keeps what was typed when it fails.
pub(super) async fn change(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, change: Change) {
    let result = match change {
        Change::Add(details) => client.add_event(&details).await.map(|_| ()),
        Change::Approve(record) => client.approve_event(&record).await,
        Change::Delete(id) => client.delete_event(&id).await,
    };
    // The server's `channel_changed` reloads the directory, and the events with it.
    let result = result.map_err(|error| {
        tracing::warn!(error = %format_args!("{error:#}"), "event change failed");
        problem(&error).to_owned()
    });
    ctx.send(Update::Calendar(url.clone(), result));
}

fn problem(error: &anyhow::Error) -> &'static str {
    let Some(EventRefused { status, code }) = error.downcast_ref::<EventRefused>() else {
        return t!("events.failed");
    };
    match code.as_str() {
        "events_disabled" => t!("events.disabled"),
        "suggestion_limit" => t!("events.suggestion_limit"),
        "event_limit" => t!("events.event_limit"),
        "event_ended" => t!("events.ended_already"),
        "event_not_found" | "event_approved" => t!("events.changed"),
        "invalid_event_time" => t!("events.bad_range"),
        "epoch_changed" => t!("admin.key_changing"),
        _ if *status == StatusCode::FORBIDDEN => t!("admin.no_permission"),
        _ => t!("events.failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusttalk_core::messages::MessageError;

    #[test]
    fn historical_event_waits_for_its_own_epoch() -> Result<()> {
        use rusttalk_core::{
            auth::{decode_bytes, encode},
            calendar::{EventContext, GRACE},
            identity::DeviceIdentity,
            keys::random_key,
        };

        let (_, identity) = DeviceIdentity::create("calendar test")?;
        let key = random_key()?;
        let details = Details {
            title: "Historical event".into(),
            start: 100,
            end: 200,
            ..Default::default()
        };
        let cert = &identity.certificate;
        let context = EventContext {
            server_id: [4; 32],
            id: [1; 32],
            author: *cert.person_id(),
            author_device: cert.signing_key().to_bytes(),
            epoch: 1,
            remove_at: details.end + GRACE,
        };
        let record = EventRecord {
            id: context.id,
            author: context.author,
            author_device: context.author_device,
            approved: true,
            epoch: context.epoch,
            remove_at: context.remove_at,
            certificate: encode(cert.public_bytes()?),
            envelope: encode(&context.seal(&key, &identity, &details)?),
        };
        let mut event = Event {
            record,
            details: None,
            waiting_for_key: false,
        };
        open(&mut event, |_| Err(MessageError::MissingKey.into()));
        let mut calendar = Calendar {
            enabled: true,
            events: vec![event],
        };
        // Names already use epoch two.
        assert!(!retry(
            &mut calendar,
            |epoch| epoch == 2,
            |_| unreachable!()
        ));
        assert!(retry(
            &mut calendar,
            |epoch| epoch == 1,
            |record| Ok(context.open(&key, cert, &decode_bytes(&record.envelope)?)?)
        ));
        assert_eq!(calendar.events[0].details.as_ref(), Some(&details));
        assert!(!retry(&mut calendar, |_| true, |_| unreachable!()));
        Ok(())
    }
}
