//! Member presence on the authenticated event stream.

use serde::{Deserialize, Serialize};

use crate::keys::Event;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Online,
    Away,
    /// Do not disturb: shown to others, and no notifications or message sounds on any
    /// of the member's devices.
    Dnd,
    Invisible,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Online,
    Away,
    /// Do not disturb. Servers send it only to apps that announce [`FEATURES`].
    Dnd,
    #[default]
    Offline,
}

/// Request header in which apps name the protocol additions they understand, comma
/// separated. Older servers ignore it; older apps don't send it, so they are told
/// `away` instead of `dnd`, which they cannot read.
pub const FEATURES_HEADER: &str = "rusttalk-features";
/// What this app understands: the `dnd` status and presence.
pub const FEATURES: &str = "dnd";

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresenceEvent {
    PresenceChanged {
        person_id: String,
        status: Presence,
        /// The member's own status choice, sent only to that member's own devices when
        /// it changes, so every device shows the latest. Older servers leave it out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chosen: Option<Status>,
    },
}

/// Events returned by the presence-aware client API.
#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Update {
    Event(Event),
    Presence(PresenceEvent),
}
