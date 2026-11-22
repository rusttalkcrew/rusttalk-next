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
    let events = (calendar.events.into_iter())
        .map(|record| {
            let details = client
                .open_event(&record)
                .inspect_err(|error| {
                    tracing::warn!(error = %format_args!("{error:#}"), "event could not be opened");
                })
                .ok();
            Event { details, record }
        })
        .collect();
    Ok(Some(Calendar {
        enabled: calendar.enabled,
        events,
    }))
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
