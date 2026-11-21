//! What the UI shows for each server. The network task in net.rs decrypts the
//! server's metadata and messages and sends these; the UI only reads and merges them.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use eframe::egui::ColorImage;

use rusttalk_core::{
    client::{
        Attachment,
        voice::{CallTraffic, TrackSid},
    },
    identity::fingerprint,
    presence::{Presence, Status as Choice},
};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::net::{Command, Compatibility};

pub type Id = [u8; 32];

pub struct Server {
    pub address: Url,
    /// The name from the invite, or the host, until the space key arrives.
    pub label: String,
    pub status: Status,
    pub compatibility: Compatibility,
    pub order_unavailable: bool,
    pub dir: Directory,
    pub chats: HashMap<Id, Chat>,
    /// The channel shown when this server is picked.
    pub open: Option<Id>,
    /// Who is in each voice channel.
    pub voice: HashMap<Id, Vec<Peer>>,
    /// Every member name seen this session, for people who have left (bans, audit log).
    pub names: HashMap<Id, String>,
    pub commands: UnboundedSender<Command>,
}

#[derive(Clone)]
pub struct Peer {
    pub person: Id,
    /// In the room on the server, but their audio isn't flowing (yet or again).
    pub connecting: bool,
    /// A device the server couldn't match to a member; `person` is all zeros.
    pub unknown: bool,
    pub status: rusttalk_core::client::voice::VoiceStatus,
    /// Track and name of each app they share, only known inside the room.
    pub apps: Vec<(TrackSid, String)>,
    /// Screens they share, only known inside the room.
    pub screens: Vec<Screen>,
    /// Their camera track, only known inside the room.
    pub camera: Option<String>,
}

#[derive(Clone, PartialEq)]
pub struct Screen {
    /// The track, to watch it.
    pub sid: String,
    pub name: String,
}

impl Peer {
    pub fn name(&self, server: &Server) -> String {
        if self.unknown {
            "Unknown device".into()
        } else {
            server.member_name(&self.person)
        }
    }
}

/// The voice room this device is in.
pub struct Call {
    pub server: Url,
    pub channel: Id,
    pub state: CallState,
    pub stats: Stats,
    /// False when this device may only listen.
    pub can_talk: bool,
    /// False when this device may not publish a screen or camera.
    pub can_share_video: bool,
}

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
    /// Packets lost since joining.
    pub lost_in: Option<i64>,
    pub lost_out: Option<i64>,
    /// Packets received since joining, to turn `lost_in` into a rate.
    pub received_in: Option<u64>,
    pub tracks: Vec<TrackStats>,
    pub traffic: CallTraffic,
}

#[derive(Clone)]
pub struct TrackStats {
    pub person: Id,
    /// Sent by this device, rather than received.
    pub mine: bool,
    /// The shared app, for an app track.
    pub app: Option<String>,
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
        match self.names.get(person) {
            Some(name) if !self.dir.members.iter().any(|m| m.person == *person) => name.clone(),
            _ => member_name(&self.dir, person),
        }
    }

    pub fn member(&self, person: &Id) -> Option<&Member> {
        self.dir.members.iter().find(|m| m.person == *person)
    }

    pub fn send(&self, command: Command) {
        // The task only ends after the UI drops this sender or the server stopped for good.
        let _ = self.commands.send(command);
    }

    pub fn channel(&self, id: &Id) -> Option<&Channel> {
        self.dir
            .categories
            .iter()
            .flat_map(|category| &category.channels)
            .find(|channel| channel.id == *id)
    }

    pub fn is_voice(&self, id: &Id) -> bool {
        self.channel(id).is_some_and(|channel| channel.voice)
    }

    /// Find a text channel or DM by ID: (title, is a DM).
    pub fn chat_title(&self, id: &Id) -> Option<(String, bool)> {
        if let Some(dm) = self.dir.dms.iter().find(|dm| dm.channel == *id) {
            return Some((self.member_name(&dm.peer), true));
        }
        self.channel(id)
            .map(|channel| (channel.name.clone(), false))
    }
}

pub fn member_name(dir: &Directory, person: &Id) -> String {
    match dir.members.iter().find(|member| member.person == *person) {
        Some(member) => member.display_name(),
        None => format!("Member {}", &fingerprint(person)[..4]),
    }
}

#[derive(Clone, Default)]
pub struct Directory {
    pub name: Option<String>,
    pub icon: Option<Picture>,
    /// This person's presence choice here.
    pub status: Option<Choice>,
    pub me: Id,
    pub perms: Perms,
    pub categories: Vec<Category>,
    pub members: Vec<Member>,
    pub dms: Vec<Dm>,
    /// Highest rank first.
    pub roles: Vec<Role>,
    /// The role new members get.
    pub default_role: String,
    /// Server emoji and stickers, oldest first.
    pub emoji: Vec<CustomEmoji>,
    /// How many emoji and stickers the server allows.
    pub emoji_max: (usize, usize),
}

/// A server emoji or sticker, its name decrypted here.
#[derive(Clone)]
pub struct CustomEmoji {
    pub id: Id,
    pub name: String,
    pub sticker: bool,
    /// `None` until the image is fetched, or when it could not be read.
    pub picture: Option<Picture>,
}

impl Directory {
    pub fn emoji_by_id(&self, id: &Id) -> Option<&CustomEmoji> {
        self.emoji.iter().find(|e| e.id == *id)
    }
}

#[derive(Clone, Copy, Default)]
pub struct Perms {
    pub owner: bool,
    /// Has the built-in Admin role (or is the owner): may change who can join.
    pub admin: bool,
    pub manage_channels: bool,
    pub invite: bool,
    pub delete_messages: bool,
    pub kick: bool,
    pub ban: bool,
    pub manage_roles: bool,
    pub audit: bool,
    pub manage_server: bool,
    pub manage_emoji: bool,
    /// This person's highest role rank; they can only manage people and roles below it.
    pub rank: i64,
}

impl Perms {
    /// May open the server settings window.
    pub fn settings(&self) -> bool {
        self.admin
            || self.invite
            || self.kick
            || self.ban
            || self.manage_roles
            || self.audit
            || self.manage_server
            || self.manage_emoji
    }
}

pub const MENTION_EVERYONE: &str = "mention_everyone";

/// The permissions a role can grant, as the API names them, with their labels.
pub const PERMISSIONS: [(&str, &str); 10] = [
    ("invite", "Invite people"),
    ("kick", "Kick"),
    ("ban", "Ban"),
    ("manage_roles", "Manage roles"),
    ("view_audit_log", "See audit log"),
    ("delete_messages", "Delete messages"),
    ("manage_channels", "Manage channels"),
    ("manage_server", "Manage server"),
    (MENTION_EVERYONE, "Mention everyone"),
    ("manage_emoji", "Manage emoji"),
];

#[derive(Clone)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub name_known: bool,
    pub rank: i64,
    /// API permission names, see `PERMISSIONS`.
    pub permissions: Vec<String>,
    /// Made by people, not one of the four built-in roles.
    pub custom: bool,
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
    pub topic: String,
    pub voice: bool,
    /// Per role: read (1), write (2), speak (4) and video (8).
    pub flags: Vec<(String, u8)>,
}

#[derive(Clone)]
pub struct Member {
    pub person: Id,
    pub name: Option<String>,
    pub devices: Vec<Device>,
    pub roles: Vec<String>,
    pub avatar: Option<Picture>,
    pub presence: Presence,
}

impl Member {
    /// Their chosen name, or a placeholder from their fingerprint.
    pub fn display_name(&self) -> String {
        (self.name.clone()).unwrap_or_else(|| format!("Member {}", &fingerprint(&self.person)[..4]))
    }
}

/// A decoded profile picture or server icon: each frame and how long it shows, in ms.
#[derive(Clone)]
pub struct Picture {
    /// A hash of the image file, to keep its textures.
    pub key: [u8; 32],
    pub frames: Arc<Vec<(ColorImage, u32)>>,
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
    /// Server-stored files; edits never change them.
    pub files: Vec<Attachment>,
    /// A file offered live: transfer ID, name and size.
    pub live: Option<(Id, String, u64)>,
    /// A server sticker by ID; the text is its name.
    pub sticker: Option<Id>,
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

pub struct Staged {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
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
    pub mentions: crate::home::mentions::DraftMentions,
    pub reply_to: Option<Id>,
    pub editing: Option<Id>,
    /// Files picked or dropped, sent with the next message.
    pub staged: Vec<Staged>,
    /// What is uploading now and how far (0 to 1), shown above the composer.
    pub uploading: Option<(String, f32)>,
    /// Content height last frame, and before older messages were added,
    /// so the view stays in place when a page is added above it.
    pub content_height: f32,
    pub keep_scroll: Option<f32>,
    pub scroll_to: Option<f32>,
}

impl Chat {
    pub fn open_draft(&mut self, dir: &Directory, text: &str) {
        (self.draft, self.mentions) = crate::home::mentions::DraftMentions::open(dir, text);
    }

    pub fn clear_draft(&mut self) {
        self.draft.clear();
        self.mentions = Default::default();
    }

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
        // A retried edit replaces its "couldn't be opened" placeholder.
        self.messages.retain(|msg| {
            msg.body != Body::Unreadable || !edits.iter().any(|edit| edit.id == msg.id)
        });
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
            // Wait for an unreadable original to be retried, then apply.
            if target.body == Body::Unreadable {
                return true;
            }
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
            files: Vec::new(),
            live: None,
            sticker: None,
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

    #[test]
    fn retried_records_replace_their_unreadable_placeholders() {
        let unreadable = |id: u8, at: i64| Msg {
            body: Body::Unreadable,
            ..msg(id, at, "", None)
        };
        let mut chat = Chat::default();
        chat.merge(vec![unreadable(1, 10), unreadable(2, 20)], false);
        // The edit opens now, but its original still can't.
        chat.merge(vec![msg(2, 20, "a2", Some(1))], false);
        assert_eq!(texts(&chat), ["?"]);
        // The original opens on retry and gets the waiting edit.
        chat.merge(vec![msg(1, 10, "a", None)], false);
        assert_eq!(texts(&chat), ["a2"]);
    }
}
