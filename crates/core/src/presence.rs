//! Member presence on the authenticated event stream.

use serde::{Deserialize, Serialize};

use crate::keys::Event;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Online,
    Away,
    Invisible,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Online,
    Away,
    #[default]
    Offline,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresenceEvent {
    PresenceChanged { person_id: String, status: Presence },
}

/// Events returned by the presence-aware client API.
#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Update {
    Event(Event),
    Presence(PresenceEvent),
}
