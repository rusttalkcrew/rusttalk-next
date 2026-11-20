//! What the UI shows for each server. The network task in net.rs decrypts the
//! server's metadata and messages and sends these; the UI only reads and merges them.

use std::collections::HashMap;

use rusttalk_core::identity::fingerprint;
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::net::Command;

pub type Id = [u8; 32];

pub struct Server {
    pub address: Url,
    /// The name from the invite, or the host, until the space key arrives.
    pub label: String,
    pub status: Status,
    pub dir: Directory,
    pub chats: HashMap<Id, Chat>,
    /// The channel shown when this server is picked.
    pub open: Option<Id>,
    /// Who is in each voice channel.
    pub voice: HashMap<Id, Vec<Peer>>,
    pub commands: UnboundedSender<Command>,
}

#[derive(Clone)]
pub struct Peer {
    pub person: Id,
    /// In the room on the server, but their audio isn't flowing (yet or again).
    pub connecting: bool,
}

/// The voice room this device is in.
pub struct Call {
    pub server: Url,
    pub channel: Id,
    pub state: CallState,
    pub stats: Stats,
}

#[expect(dead_code, reason = "used once voice is wired to the core module")]
#[derive(Clone, Copy, PartialEq)]
pub enum CallState {
    Connecting,
    Connected,
    /// Lost the media connection; retrying by itself.
    Reconnecting,
}

/// From the SDK's WebRTC stats; `None` until it reports a value.
#[derive(Clone, Default)]
pub struct Stats {
    pub ping_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    /// Percent of packets lost.
    pub loss_in: Option<f64>,
    pub loss_out: Option<f64>,
    pub tracks: Vec<TrackStats>,
}

#[derive(Clone)]
pub struct TrackStats {
    pub person: Id,
    /// Sent by this device, rather than received.
    pub mine: bool,
    pub kbps: f64,
}

#[derive(Clone, PartialEq)]
pub enum Status {
    Connecting,
    Online,
    /// Retrying in the background.
    Offline,
    /// The task stopped and will not retry.
    Stopped(String),
}

impl Server {
    pub fn name(&self) -> &str {
        self.dir.name.as_deref().unwrap_or(&self.label)
    }

    pub fn member_name(&self, person: &Id) -> String {
        member_name(&self.dir, person)
    }

    pub fn send(&self, command: Command) {
        // The task only ends after the UI drops this sender or the server stopped for good.
        let _ = self.commands.send(command);
    }

    pub fn is_voice(&self, id: &Id) -> bool {
        self.dir
            .categories
            .iter()
            .flat_map(|category| &category.channels)
            .any(|channel| channel.id == *id && channel.voice)
    }

    /// Find a text channel or DM by ID: (title, is a DM).
    pub fn chat_title(&self, id: &Id) -> Option<(String, bool)> {
        if let Some(dm) = self.dir.dms.iter().find(|dm| dm.channel == *id) {
            return Some((self.member_name(&dm.peer), true));
        }
        self.dir
            .categories
            .iter()
            .flat_map(|category| &category.channels)
            .find(|channel| channel.id == *id)
            .map(|channel| (channel.name.clone(), false))
    }
}

pub fn member_name(dir: &Directory, person: &Id) -> String {
    dir.members
        .iter()
        .find(|member| member.person == *person)
        .and_then(|member| member.name.clone())
        .unwrap_or_else(|| format!("Member {}", &fingerprint(person)[..4]))
}

#[derive(Clone, Default)]
pub struct Directory {
    pub name: Option<String>,
    pub me: Id,
    pub perms: Perms,
    pub categories: Vec<Category>,
    pub members: Vec<Member>,
    pub dms: Vec<Dm>,
}

#[derive(Clone, Copy, Default)]
pub struct Perms {
    pub owner: bool,
    pub manage_channels: bool,
    pub invite: bool,
    pub delete_messages: bool,
}

/// Channels without a category go in a category without an ID or name.
#[derive(Clone)]
pub struct Category {
    pub id: Option<Id>,
    pub name: String,
    pub channels: Vec<Channel>,
}

#[derive(Clone)]
pub struct Channel {
    pub id: Id,
    pub name: String,
    pub voice: bool,
}

#[derive(Clone)]
pub struct Member {
    pub person: Id,
    pub name: Option<String>,
    pub devices: Vec<Device>,
}

#[derive(Clone)]
pub struct Device {
    pub name: String,
    pub fingerprint: String,
}

#[derive(Clone)]
pub struct Dm {
    pub channel: Id,
    pub peer: Id,
}

#[derive(Clone)]
pub struct Msg {
    pub id: Id,
    pub person: Id,
    pub at: i64,
    pub reply_to: Option<Id>,
    pub edit_of: Option<Id>,
    pub body: Body,
}

#[derive(Clone, PartialEq)]
pub enum Body {
    Text(String),
    /// Could not be verified or decrypted (yet).
    Unreadable,
    /// A member or device change, with fingerprints for the details view.
    /// `name` is the person's name when it happened, used once they have left.
    Notice {
        name: String,
        action: &'static str,
        detail: String,
    },
}

/// One channel's loaded history, oldest first, plus the composer state.
#[derive(Default)]
pub struct Chat {
    pub messages: Vec<Msg>,
    /// Originals that show edited text, with the time of the applied edit.
    pub edited: HashMap<Id, i64>,
    /// Edits whose original is not loaded yet.
    pending_edits: HashMap<Id, Msg>,
    pub loading: bool,
    pub at_start: bool,
    pub draft: String,
    pub reply_to: Option<Id>,
    pub editing: Option<Id>,
    /// Content height last frame, and before older messages were added,
    /// so the view stays in place when a page is added above it.
    pub content_height: f32,
    pub keep_scroll: Option<f32>,
    pub scroll_to: Option<f32>,
}

impl Chat {
    /// Add records from a page or a live event, applying edits to their originals.
    pub fn merge(&mut self, items: Vec<Msg>, older: bool) {
        let (edits, plain): (Vec<_>, Vec<_>) =
            items.into_iter().partition(|msg| msg.edit_of.is_some());
        let mut fresh = Vec::new();
        for msg in plain {
            match self.messages.iter_mut().find(|old| old.id == msg.id) {
                // A retry after a missing key replaces the placeholder.
                Some(old) if old.body == Body::Unreadable => *old = msg,
                Some(_) => {}
                None => fresh.push(msg),
            }
        }
        if older {
            self.messages.splice(0..0, fresh);
        } else {
            self.messages.extend(fresh);
        }
        // Stable, so records from the same second keep the server's order.
        self.messages.sort_by_key(|msg| msg.at);
        for edit in edits {
            let original = edit.edit_of.expect("partitioned on edit_of");
            if self
                .pending_edits
                .get(&original)
                .is_none_or(|old| old.at <= edit.at)
            {
                self.pending_edits.insert(original, edit);
            }
        }
        self.pending_edits.retain(|original, edit| {
            let Some(target) = self.messages.iter_mut().find(|msg| msg.id == *original) else {
                return true;
            };
            if let (Body::Text(text), Body::Text(new)) = (&mut target.body, &edit.body)
                && self.edited.get(original).is_none_or(|at| *at <= edit.at)
            {
                text.clone_from(new);
                self.edited.insert(*original, edit.at);
            }
            false
        });
    }

    /// Drop a deleted record. Edits of a deleted original stay hidden.
    // shortcut: deleting an edit record keeps its text on the original, until a reload.
    pub fn remove(&mut self, id: &Id) {
        self.messages.retain(|msg| msg.id != *id);
        self.pending_edits.remove(id);
        self.edited.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: u8, at: i64, text: &str, edit_of: Option<u8>) -> Msg {
        Msg {
            id: [id; 32],
            person: [9; 32],
            at,
            reply_to: None,
            edit_of: edit_of.map(|id| [id; 32]),
            body: Body::Text(text.into()),
        }
    }

    fn texts(chat: &Chat) -> Vec<&str> {
        chat.messages
            .iter()
            .map(|msg| match &msg.body {
                Body::Text(text) => text.as_str(),
                _ => "?",
            })
            .collect()
    }

    #[test]
    fn merges_pages_dedupes_and_applies_the_newest_edit() {
        let mut chat = Chat::default();
        // The newest page holds an edit whose original is on an older page.
        chat.merge(
            vec![msg(3, 30, "c", None), msg(4, 40, "a2", Some(1))],
            false,
        );
        assert_eq!(texts(&chat), ["c"]);
        chat.merge(vec![msg(1, 10, "a", None), msg(2, 20, "b", None)], true);
        assert_eq!(texts(&chat), ["a2", "b", "c"]);
        assert!(chat.edited.contains_key(&[1; 32]));
        // An older edit arriving later does not win, and duplicates are ignored.
        chat.merge(
            vec![msg(5, 15, "a1", Some(1)), msg(3, 30, "c", None)],
            false,
        );
        assert_eq!(texts(&chat), ["a2", "b", "c"]);
        chat.remove(&[1; 32]);
        assert_eq!(texts(&chat), ["b", "c"]);
        assert!(!chat.edited.contains_key(&[1; 32]));
    }
}
