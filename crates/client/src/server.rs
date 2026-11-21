//! What the UI shows for each server. The network task in net.rs decrypts the
//! server's metadata and messages and sends these; the UI only reads and merges them.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
};

use eframe::egui::{Color32, ColorImage};

use rusttalk_core::{
    auth::KnownServer,
    client::{
        Attachment,
        voice::{CallTraffic, TrackSid},
    },
    identity::fingerprint,
    presence::{Presence, Status as Choice},
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::net::{Command, Compatibility};

pub type Id = [u8; 32];

pub struct Server {
    pub address: Url,
    /// The pinned server ID and certificate, so an invite is only taken as this
    /// server's when it names the same identity, not just the same address.
    pub known: KnownServer,
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
    /// The device (LiveKit identity) sharing a terminal, only known inside the room.
    pub terminal: Option<String>,
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
            t!("server.unknown_device").into()
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
    /// The track, to find a tile's numbers.
    pub sid: String,
    /// Sent by this device, rather than received.
    pub mine: bool,
    /// Our own screen share.
    pub screen: bool,
    /// The shared app, for an app track.
    pub app: Option<String>,
    pub kbps: f64,
    pub video: Option<rusttalk_core::client::voice::VideoFormat>,
    pub detail: Option<rusttalk_core::client::voice::VideoDetail>,
    pub jitter_ms: Option<f64>,
    /// Packets lost and received since joining, to turn them into a rate.
    pub lost: Option<i64>,
    pub received: Option<u64>,
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

    pub fn voice_muted(&self, person: &Id) -> bool {
        self.member(person).is_some_and(|member| member.voice_muted)
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
        None => t!("server.member_unnamed", id = &fingerprint(person)[..4]),
    }
}

#[derive(Clone, Default)]
pub struct Directory {
    pub name: Option<String>,
    pub icon: Option<Picture>,
    /// This person's presence choice here.
    pub status: Option<Choice>,
    /// Whether this person hides from member lists here while offline.
    pub hide_offline: Option<bool>,
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
    pub mute_members: bool,
    /// Remove people from a voice channel or move them to another.
    pub move_members: bool,
    /// This person's highest role rank; they can only manage people and roles below it.
    pub rank: i64,
}

pub const MENTION_EVERYONE: &str = "mention_everyone";

#[derive(Clone)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub name_known: bool,
    pub rank: i64,
    /// API permission names, see `permissions`.
    pub permissions: Vec<String>,
    /// Made by people, not one of the four built-in roles.
    pub custom: bool,
    pub color: Option<Color32>,
    /// Its members are listed under its own heading in the member list.
    pub separate: bool,
}

impl Directory {
    /// The colour of someone's highest coloured role, for their name.
    pub fn name_color(&self, roles: &[String]) -> Option<Color32> {
        (self.roles.iter())
            .filter(|role| roles.contains(&role.id))
            .find_map(|role| role.color)
    }

    /// The highest role listed separately that someone has, if any.
    pub fn heading_role(&self, roles: &[String]) -> Option<&Role> {
        (self.roles.iter()).find(|role| role.separate && roles.contains(&role.id))
    }
}

/// A role's colour and grouping as sealed for the server. Version 1 is
/// `[1, flags, r, g, b]`, flag 1 for a colour and 2 for listed separately.
pub fn role_display(color: Option<Color32>, separate: bool) -> Vec<u8> {
    let [r, g, b, _] = color.unwrap_or_default().to_array();
    let flags = u8::from(color.is_some()) | u8::from(separate) << 1;
    vec![1, flags, r, g, b]
}

/// Read `role_display`; a newer version than this client knows shows no colour.
pub fn parse_role_display(bytes: &[u8]) -> (Option<Color32>, bool) {
    match bytes {
        [1, flags, r, g, b, ..] => (
            (flags & 1 != 0).then(|| Color32::from_rgb(*r, *g, *b)),
            flags & 2 != 0,
        ),
        _ => (None, false),
    }
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
    /// Opus bitrate of a voice channel, in kbps.
    pub audio_kbps: u32,
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
    /// They asked to be left out of member lists while offline.
    pub hide_offline: bool,
    /// A moderator muted them in every voice channel.
    pub voice_muted: bool,
}

impl Member {
    /// Whether the member list shows them; hidden people still have names elsewhere.
    pub fn listed(&self) -> bool {
        !(self.hide_offline && self.presence == Presence::Offline)
    }

    /// Their chosen name, or a placeholder from their fingerprint.
    pub fn display_name(&self) -> String {
        (self.name.clone()).unwrap_or_else(|| {
            t!(
                "server.member_unnamed",
                id = &fingerprint(&self.person)[..4]
            )
        })
    }
}

/// "Hide me when I'm offline": each server stores the flag, and this device keeps
/// its last known value plus the servers it still has to tell about a change made
/// here. Saved, so a restart doesn't let a stale server undo that change.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HideOffline {
    /// What the switch shows: the last value chosen here or reported by a server.
    pub on: bool,
    /// Servers that haven't confirmed a value from this device yet, with that value.
    pub pending: BTreeMap<String, bool>,
}

impl HideOffline {
    /// The person changed it here: every server has to be told.
    pub fn choose<'a>(&mut self, on: bool, servers: impl Iterator<Item = &'a Url>) {
        self.on = on;
        self.pending = servers.map(|server| (server.to_string(), on)).collect();
    }

    /// This person joined a server, which starts with hiding off: tell it the
    /// value still being sent elsewhere, or else the one shown.
    pub fn joined(&mut self, server: &Url) {
        let on = self.pending.values().next().copied().unwrap_or(self.on);
        self.pending.insert(server.to_string(), on);
    }

    /// A server this person is no longer in has nothing left to be told.
    pub fn left(&mut self, server: &Url) {
        self.pending.remove(server.as_str());
    }

    /// A server reported its stored flag; returns what to send it, if anything.
    /// Without a value pending for that server, its value is the newest one (made
    /// on another device) and is taken over, so two devices never write back and forth.
    pub fn reported(&mut self, server: &Url, theirs: bool) -> Option<bool> {
        match self.pending.get(server.as_str()) {
            None => {
                self.on = theirs;
                None
            }
            Some(&on) if on == theirs => {
                self.pending.remove(server.as_str());
                None
            }
            Some(&on) => Some(on),
        }
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
    /// In a DM: the servers holding a copy, the shown copy's server first.
    /// Empty in channels.
    pub replicas: Vec<Url>,
    /// In a DM: each other copy's stored files, by server. Every copy has its own
    /// blobs, so downloads switch to these when the shown copy's server drops it.
    pub copy_files: Vec<(Url, Vec<Attachment>)>,
}

#[derive(Clone, PartialEq)]
pub enum Body {
    Text(String),
    /// A friend request or answer; shown as a card or a short line, not as text.
    /// A friend message, with the sender's signed send time in milliseconds.
    Friend(rusttalk_core::client::FriendSignal, u64),
    /// A poke and its note; listed under Pokes, not shown in the chat.
    Poke(String),
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

/// Which of two edits of one message wins: one with an authenticated revision beats
/// one without, then the higher revision (or, for old apps' edits, the later server
/// time), then the higher edit ID so every reader picks the same one.
pub type Rank = (bool, i64, Id);

/// An edit's reply-to field carries its revision, because current apps ignore it on
/// edits: 24 zero bytes, then the revision as a big-endian u64. It is inside the
/// signed ciphertext, so a server can't change it.
pub fn revision_id(revision: u64) -> Id {
    let mut id = [0; 32];
    id[24..].copy_from_slice(&revision.to_be_bytes());
    id
}

fn rank(edit: &Msg) -> Rank {
    match edit.reply_to {
        Some(id) if id[..24] == [0; 24] => {
            let revision = u64::from_be_bytes(id[24..].try_into().expect("8 bytes"));
            (true, i64::try_from(revision).unwrap_or(i64::MAX), edit.id)
        }
        _ => (false, edit.at, edit.id),
    }
}

/// One channel's loaded history, oldest first, plus the composer state.
#[derive(Default)]
pub struct Chat {
    pub messages: Vec<Msg>,
    /// Originals that show edited text, by person and ID, with the applied edit's rank.
    pub edited: HashMap<(Id, Id), Rank>,
    /// Edits whose original is not loaded yet, by person and original ID.
    pending_edits: HashMap<(Id, Id), Msg>,
    pub loading: bool,
    pub at_start: bool,
    pub draft: String,
    pub mentions: crate::home::mentions::DraftMentions,
    /// The message replied to: person and ID.
    pub reply_to: Option<(Id, Id)>,
    /// One of this person's own messages.
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
        let mut fresh: Vec<Msg> = Vec::new();
        for msg in plain {
            // The person is part of the key: in a DM another server could hold their
            // record under an ID of yours.
            let same = |old: &&mut Msg| old.id == msg.id && old.person == msg.person;
            let Some(old) = (self.messages.iter_mut().chain(&mut fresh)).find(same) else {
                fresh.push(msg);
                continue;
            };
            match (old.body == Body::Unreadable, msg.body == Body::Unreadable) {
                // A retry after a missing key, or a readable DM copy, replaces the
                // placeholder whole: an unopened record's time and server don't count.
                (true, false) => *old = msg,
                // DM copies on several servers: keep the earliest server time.
                (false, false) => {
                    old.at = old.at.min(msg.at);
                    for server in &msg.replicas {
                        if !old.replicas.contains(server) {
                            old.copy_files.push((server.clone(), msg.files.clone()));
                        }
                    }
                    add_replicas(&mut old.replicas, msg.replicas);
                }
                // So a delete on that server clears the placeholder.
                (true, true) => add_replicas(&mut old.replicas, msg.replicas),
                // A copy that couldn't be opened changes nothing about a readable one.
                (false, true) => {}
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
            msg.body != Body::Unreadable
                || !(edits.iter()).any(|edit| edit.id == msg.id && edit.person == msg.person)
        });
        for edit in edits {
            let key = (edit.person, edit.edit_of.expect("partitioned on edit_of"));
            if (self.pending_edits.get(&key)).is_none_or(|old| rank(old) <= rank(&edit)) {
                self.pending_edits.insert(key, edit);
            }
        }
        self.pending_edits.retain(|key, edit| {
            let Some(target) =
                (self.messages.iter_mut()).find(|msg| msg.id == key.1 && msg.person == key.0)
            else {
                return true;
            };
            // Wait for an unreadable original to be retried, then apply.
            if target.body == Body::Unreadable {
                return true;
            }
            if let (Body::Text(text), Body::Text(new)) = (&mut target.body, &edit.body)
                && self.edited.get(key).is_none_or(|old| *old <= rank(edit))
            {
                text.clone_from(new);
                self.edited.insert(*key, rank(edit));
            }
            false
        });
    }

    /// The revision for this person's next edit of a message: their clock in
    /// milliseconds, but always above the edit shown now.
    pub fn next_revision(&self, person: Id, original: Id, now_ms: u64) -> u64 {
        match self.edited.get(&(person, original)) {
            Some((true, shown, _)) => now_ms.max((*shown as u64).saturating_add(1)),
            _ => now_ms,
        }
    }

    /// The message a reply quotes. `None` while it isn't loaded, or when two people's
    /// messages share the ID, since a reply names only the ID.
    pub fn quoted(&self, id: &Id) -> Option<&Msg> {
        let mut found = self.messages.iter().filter(|msg| msg.id == *id);
        let first = found.next()?;
        found.all(|msg| msg.person == first.person).then_some(first)
    }

    /// Drop a deleted record. Edits of a deleted original stay hidden.
    // shortcut: deleting an edit record keeps its text on the original, until a reload.
    pub fn remove(&mut self, id: &Id) {
        self.messages.retain(|msg| msg.id != *id);
        self.pending_edits.retain(|(_, original), _| original != id);
        self.edited.retain(|(_, original), _| original != id);
    }

    /// Drop one person's record, leaving another person's under the same ID.
    pub fn remove_one(&mut self, person: &Id, id: &Id) {
        let key = (*person, *id);
        self.messages
            .retain(|msg| msg.id != *id || msg.person != *person);
        self.pending_edits.remove(&key);
        self.edited.remove(&key);
    }
}

fn add_replicas(replicas: &mut Vec<Url>, more: Vec<Url>) {
    for server in more {
        if !replicas.contains(&server) {
            replicas.push(server);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HideOffline;

    /// A server that stores the flag, like the API.
    fn server(n: u8) -> url::Url {
        format!("https://server{n}.test/").parse().unwrap()
    }

    #[test]
    fn two_devices_converge_without_writing_back_and_forth() {
        let a = server(1);
        let (mut first, mut second) = (HideOffline::default(), HideOffline::default());
        first.choose(true, [&a].into_iter());
        let mut stored = first.on;
        // The write tells everyone to reload; both devices read the server again.
        let mut writes = 0;
        for _ in 0..3 {
            for device in [&mut first, &mut second] {
                if let Some(on) = device.reported(&a, stored) {
                    stored = on;
                    writes += 1;
                }
            }
        }
        assert_eq!(writes, 0);
        assert!(stored && first.on && second.on);
        assert!(first.pending.is_empty());
    }

    #[test]
    fn a_change_made_offline_survives_a_restart_and_a_stale_server() {
        let (a, b) = (server(1), server(2));
        let mut device = HideOffline::default();
        device.choose(true, [&a, &b].into_iter());
        // A took it; B was offline.
        assert_eq!(device.reported(&a, true), None);
        let saved = serde_json::to_string(&device).unwrap();
        let mut restarted: HideOffline = serde_json::from_str(&saved).unwrap();
        // B answers first after the restart, with its old value: it gets told again.
        assert_eq!(restarted.reported(&b, false), Some(true));
        assert!(restarted.on);
        assert_eq!(restarted.reported(&a, true), None);
        assert_eq!(restarted.reported(&b, true), None);
        assert!(restarted.pending.is_empty());
    }

    #[test]
    fn a_new_server_gets_the_value_still_being_sent_elsewhere() {
        let (a, b, c) = (server(1), server(2), server(3));
        let mut device = HideOffline::default();
        device.choose(true, [&a, &b].into_iter());
        assert_eq!(device.reported(&a, true), None);
        // B is still offline when C is joined; C starts with hiding off.
        device.joined(&c);
        assert_eq!(device.reported(&c, false), Some(true));
        assert!(device.on);
        assert_eq!(device.reported(&b, false), Some(true));
        assert_eq!(device.reported(&c, true), None);
        // Leaving B drops what it still had to be told.
        device.left(&b);
        assert!(device.pending.is_empty());
    }

    #[test]
    fn hidden_members_leave_the_list_only_while_offline() {
        use rusttalk_core::presence::Presence;
        let member = |hide_offline, presence| super::Member {
            person: [0; 32],
            name: None,
            devices: Vec::new(),
            roles: Vec::new(),
            avatar: None,
            presence,
            hide_offline,
            voice_muted: false,
        };
        assert!(member(false, Presence::Offline).listed());
        assert!(member(true, Presence::Online).listed());
        assert!(member(true, Presence::Away).listed());
        // Invisible people look offline, so they are left out too.
        assert!(!member(true, Presence::Offline).listed());
    }

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
            replicas: Vec::new(),
            copy_files: Vec::new(),
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
        assert!(chat.edited.contains_key(&([9; 32], [1; 32])));
        // An older edit arriving later does not win, and duplicates are ignored.
        chat.merge(
            vec![msg(5, 15, "a1", Some(1)), msg(3, 30, "c", None)],
            false,
        );
        assert_eq!(texts(&chat), ["a2", "b", "c"]);
        chat.remove(&[1; 32]);
        assert_eq!(texts(&chat), ["b", "c"]);
        assert!(!chat.edited.contains_key(&([9; 32], [1; 32])));
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

    fn revised(id: u8, at: i64, text: &str, original: u8, revision: u64) -> Msg {
        Msg {
            reply_to: Some(revision_id(revision)),
            ..msg(id, at, text, Some(original))
        }
    }

    #[test]
    fn a_server_cant_roll_back_an_edit_with_its_timestamps() {
        let mut chat = Chat::default();
        chat.merge(vec![msg(1, 10, "a", None)], false);
        chat.merge(vec![revised(3, 30, "a2", 1, 2_000)], false);
        // An older edit, dated later by a server, arrives afterwards.
        chat.merge(vec![revised(2, 99, "a1", 1, 1_000)], false);
        assert_eq!(texts(&chat), ["a2"]);
        // In either order, also while the original isn't loaded yet.
        let mut chat = Chat::default();
        chat.merge(vec![revised(2, 99, "a1", 1, 1_000)], false);
        chat.merge(vec![revised(3, 30, "a2", 1, 2_000)], false);
        chat.merge(vec![msg(1, 10, "a", None)], true);
        assert_eq!(texts(&chat), ["a2"]);
        // An old app's edit, ordered by server time, never beats a revised one.
        chat.merge(vec![msg(4, 999, "old app", Some(1))], false);
        assert_eq!(texts(&chat), ["a2"]);
        // The next edit's revision is above the shown one, even with a slow clock.
        assert_eq!(chat.next_revision([9; 32], [1; 32], 5), 2_001);
        assert_eq!(chat.next_revision([9; 32], [1; 32], 9_000), 9_000);
        assert_eq!(chat.next_revision([9; 32], [2; 32], 5), 5);
    }

    #[test]
    fn edits_and_replies_follow_the_person_as_well_as_the_id() {
        let theirs = |m: Msg| Msg {
            person: [8; 32],
            ..m
        };
        let mut chat = Chat::default();
        chat.merge(vec![msg(1, 10, "mine", None)], false);
        assert_eq!(chat.quoted(&[1; 32]).map(|m| m.person), Some([9; 32]));
        // The other person reuses the ID and "edits" it.
        chat.merge(
            vec![
                theirs(msg(1, 11, "theirs", None)),
                theirs(revised(2, 12, "forged", 1, u64::MAX)),
            ],
            false,
        );
        assert_eq!(texts(&chat), ["mine", "forged"]);
        assert!(!chat.edited.contains_key(&([9; 32], [1; 32])));
        // A reply naming that ID can't say whose message it was: no preview.
        assert!(chat.quoted(&[1; 32]).is_none());
        assert!(chat.quoted(&[5; 32]).is_none());
        // My own edit still applies to mine, and removing theirs leaves mine.
        chat.merge(vec![revised(3, 13, "mine2", 1, 1)], false);
        chat.remove_one(&[8; 32], &[1; 32]);
        assert_eq!(texts(&chat), ["mine2"]);
        assert!(chat.edited.contains_key(&([9; 32], [1; 32])));
    }
}
