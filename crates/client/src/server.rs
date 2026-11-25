//! What the UI shows for each server. The network task in net.rs decrypts the
//! server's metadata and messages and sends these; the UI only reads and merges them.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc, LazyLock, Mutex, PoisonError,
        mpsc::{self, TrySendError},
    },
};

use eframe::egui::{Color32, ColorImage};

use rusttalk_core::{
    auth::KnownServer,
    client::{
        Attachment,
        voice::{CallTraffic, TrackSid},
    },
    identity::fingerprint,
    messages::{Ballot, Poll, PollPart, Reaction, Tally},
    presence::{Presence, Status as Choice},
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::net::{Command, Compatibility, frame_image};

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
    /// The Events page shows while no channel is open.
    pub events_open: bool,
    pub events: crate::home::events::View,
    /// Who is in each voice channel.
    pub voice: HashMap<Id, Vec<Peer>>,
    /// Every member name seen this session, for people who have left (bans, audit log).
    pub names: HashMap<Id, String>,
    pub commands: UnboundedSender<Command>,
    /// "View server as": `dir` shows the preview, this keeps what the server sent.
    pub view_as: Option<crate::home::view_as::ViewAs>,
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
    /// The device (LiveKit identity) hosting a diagram, only known inside the room.
    pub diagram: Option<String>,
}

#[derive(Clone, PartialEq)]
pub struct Screen {
    pub viewers: usize,
    /// Captured on this device, rather than another device of the same person.
    pub local: bool,
    /// The track, to watch it.
    pub sid: String,
    pub name: String,
    /// Whether the sharer takes control requests; `None` from older apps.
    pub control: Option<rusttalk_core::client::voice::ControlOffer>,
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
    /// The first connection attempt has not finished.
    Connecting,
    Online,
    /// Retrying in the background.
    Offline,
    /// The task stopped and will not retry.
    Stopped(String),
}

impl Server {
    /// Whether this server loaded and this device can read this person's name there;
    /// until then the default-name logic leaves it alone.
    pub fn profile_ready(&self) -> bool {
        self.dir.me != [0; 32] && !self.dir.my_name_hidden
    }

    pub fn name(&self) -> &str {
        self.dir.name.as_deref().unwrap_or(&self.label)
    }

    pub fn member_name(&self, person: &Id) -> String {
        match self.names.get(person) {
            Some(name) if self.dir.members.get(person).is_none() => name.clone(),
            _ => member_name(&self.dir, person),
        }
    }

    pub fn voice_muted(&self, person: &Id) -> bool {
        self.member(person).is_some_and(|member| member.voice_muted)
    }

    /// The directory shown and, while previewing, the real one, for changes that
    /// belong in both.
    pub fn dirs_mut(&mut self) -> impl Iterator<Item = &mut Directory> {
        let real = self.view_as.as_mut().map(|view| view.real_mut());
        std::iter::once(&mut self.dir).chain(real)
    }

    pub fn member(&self, person: &Id) -> Option<&Member> {
        self.dir.members.get(person)
    }

    pub fn send(&self, command: Command) {
        if let Some(view) = &self.view_as
            && !crate::home::view_as::allowed(&command)
        {
            view.refuse();
            return;
        }
        Self::send_to(&self.commands, command);
    }

    pub fn send_to(commands: &UnboundedSender<Command>, mut command: Command) -> bool {
        crate::net::Queued::count(&mut command);
        commands.send(command).is_ok()
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

    /// Whether this person may post in a channel: one of their roles has its Write flag.
    pub fn can_write(&self, id: &Id) -> bool {
        let Some(channel) = self.channel(id) else {
            return false;
        };
        let roles = self
            .member(&self.dir.me)
            .map_or(&[][..], |m| m.roles.as_slice());
        self.dir.perms.owner
            || (channel.flags.iter()).any(|(role, flags)| flags & 2 != 0 && roles.contains(role))
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
    match dir.members.get(person) {
        Some(member) => member.display_name(),
        None => t!("server.member_unnamed", id = &fingerprint(person)[..4]),
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum SystemMessages {
    #[default]
    First,
    None,
    Channel(Id),
}

#[derive(Clone, Default)]
pub struct Directory {
    pub system_messages: SystemMessages,
    pub system_messages_supported: bool,
    pub first_text_channel: Option<Id>,
    pub name: Option<String>,
    pub icon: Option<Picture>,
    /// This person's presence choice here.
    pub status: Option<Choice>,
    /// Whether this person hides from member lists here while offline.
    pub hide_offline: Option<bool>,
    /// The server stores bios (older servers drop them).
    pub bios: bool,
    /// This person has a name here this device can't read yet (no key for its
    /// epoch), so it must not be taken for having no name.
    pub my_name_hidden: bool,
    pub me: Id,
    pub perms: Perms,
    pub categories: Vec<Category>,
    pub members: Members,
    pub dms: Vec<Dm>,
    /// Highest rank first.
    pub roles: Vec<Role>,
    /// The role new members get.
    pub default_role: String,
    /// Server emoji and stickers, oldest first.
    pub emoji: Vec<CustomEmoji>,
    /// How many emoji and stickers the server allows.
    pub emoji_max: (usize, usize),
    /// Emoji and sticker packs in display order, and how many the server allows.
    pub packs: Vec<EmojiPack>,
    pub packs_max: usize,
    /// `None` on servers from before events.
    pub calendar: Option<Calendar>,
}

/// The server's events, as far as this person may see them.
#[derive(Clone, Default)]
pub struct Calendar {
    pub enabled: bool,
    pub events: Vec<Event>,
}

#[derive(Clone)]
pub struct Event {
    pub waiting_for_key: bool,
    /// Indices into this snapshot’s answer records that need a historical space key.
    pub waiting_answers: Vec<usize>,
    /// `None` when this device could not read or verify it.
    pub details: Option<rusttalk_core::calendar::Details>,
    pub record: rusttalk_core::calendar::EventRecord,
    /// Who answered what; answers this device can't verify are left out.
    pub answers: Vec<(Id, rusttalk_core::calendar::Answer)>,
}

/// A named group of emoji and stickers.
#[derive(Clone, Debug, PartialEq)]
pub struct EmojiPack {
    pub id: Id,
    /// `None` for the Default pack nobody named: apps say "Default".
    pub name: Option<String>,
}

impl EmojiPack {
    pub fn title(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| t!("picker.default_pack").to_owned())
    }
}

/// A server emoji or sticker, its name decrypted here.
#[derive(Clone)]
pub struct CustomEmoji {
    pub id: Id,
    pub name: String,
    pub sticker: bool,
    /// The pack it is in.
    pub pack: Id,
    /// `None` until the image is fetched, or when it could not be read.
    pub picture: Option<Picture>,
}

impl Directory {
    pub fn discard_channels(&mut self, ids: &std::collections::HashSet<Id>) {
        self.dms.retain(|dm| !ids.contains(&dm.channel));
        for category in &mut self.categories {
            category
                .channels
                .retain(|channel| !ids.contains(&channel.id));
        }
    }

    pub fn text_channels(&self) -> impl Iterator<Item = &Channel> {
        self.categories
            .iter()
            .flat_map(|c| &c.channels)
            .filter(|c| !c.page && !c.voice && !c.forum)
    }

    pub fn notice_channel(&self) -> Option<Id> {
        let id = match self.system_messages {
            SystemMessages::First => self.first_text_channel?,
            SystemMessages::None => return None,
            SystemMessages::Channel(id) => id,
        };
        self.text_channels().find(|ch| ch.id == id).map(|ch| ch.id)
    }

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
    /// Suggest events, which show once approved.
    pub suggest_events: bool,
    /// Add events without approval.
    pub create_events: bool,
    /// Approve, decline and cancel anyone's events.
    pub manage_events: bool,
    /// Put a channel into focus mode for a while.
    pub start_focus: bool,
    /// This person's highest role rank; they can only manage people and roles below it.
    pub rank: i64,
    /// "View server as" is on: these are a preview, and nothing may be changed.
    pub preview: bool,
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

/// What a new channel is for.
#[derive(Clone, Copy, PartialEq)]
pub enum ChannelKind {
    Text,
    Voice,
    Page,
    Forum,
}

impl ChannelKind {
    /// The kind as the server names it.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Voice => "voice",
            Self::Page => "page",
            Self::Forum => "forum",
        }
    }

    pub fn of(channel: &Channel) -> Self {
        if channel.voice {
            Self::Voice
        } else if channel.page {
            Self::Page
        } else if channel.forum {
            Self::Forum
        } else {
            Self::Text
        }
    }
}

/// Channels without a category go in a category without an ID or name.
#[derive(Clone, Serialize, Deserialize)]
pub struct Category {
    pub id: Option<Id>,
    pub name: String,
    pub channels: Vec<Channel>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: Id,
    pub name: String,
    pub topic: String,
    pub voice: bool,
    /// Shows its newest message as one Markdown page instead of a chat.
    pub page: bool,
    /// Shows its messages as posts, each with its own replies.
    #[serde(default)]
    pub forum: bool,
    /// Opus bitrate of a voice channel, in kbps.
    pub audio_kbps: u32,
    /// Most people in a voice channel, 0 for no limit.
    #[serde(default)]
    pub member_limit: u32,
    /// While focus mode is on: when it ends, in unix seconds.
    #[serde(default)]
    pub focus_until: Option<i64>,
    /// Per role: read (1), write (2), speak (4) and video (8).
    pub flags: Vec<(String, u8)>,
}

impl Channel {
    /// When focus mode ends, if it is on at `now` (unix seconds). Only voice channels have it.
    pub fn focus(&self, now: i64) -> Option<i64> {
        self.focus_until.filter(|until| self.voice && *until > now)
    }
}

/// The member list, with an index by person so a painted row's lookup doesn't scan it.
/// Reads go through the list (`Deref`); changes go through `edit`, which reindexes.
#[derive(Clone, Default)]
pub struct Members {
    list: Vec<Member>,
    /// Each person's first row, as a scan would find it.
    index: HashMap<Id, usize>,
    /// Unique to this content: new on every change, so views can cache what they derive.
    generation: u64,
}

impl Members {
    pub fn get(&self, person: &Id) -> Option<&Member> {
        self.index.get(person).map(|&at| &self.list[at])
    }

    /// Change the list; the index is rebuilt when the returned guard drops.
    pub fn edit(&mut self) -> MembersEdit<'_> {
        MembersEdit(self)
    }

    /// Changes whenever the list may have changed; equal generations mean equal lists.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn reindex(&mut self) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        self.generation = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.index.clear();
        for (at, member) in self.list.iter().enumerate() {
            self.index.entry(member.person).or_insert(at);
        }
    }
}

impl From<Vec<Member>> for Members {
    fn from(list: Vec<Member>) -> Self {
        let mut members = Members {
            list,
            index: HashMap::new(),
            generation: 0,
        };
        members.reindex();
        members
    }
}

impl FromIterator<Member> for Members {
    fn from_iter<I: IntoIterator<Item = Member>>(iter: I) -> Self {
        Vec::from_iter(iter).into()
    }
}

impl std::ops::Deref for Members {
    type Target = [Member];

    fn deref(&self) -> &[Member] {
        &self.list
    }
}

impl<'a> IntoIterator for &'a Members {
    type Item = &'a Member;
    type IntoIter = std::slice::Iter<'a, Member>;

    fn into_iter(self) -> Self::IntoIter {
        self.list.iter()
    }
}

pub struct MembersEdit<'a>(&'a mut Members);

impl std::ops::Deref for MembersEdit<'_> {
    type Target = Vec<Member>;

    fn deref(&self) -> &Vec<Member> {
        &self.0.list
    }
}

impl std::ops::DerefMut for MembersEdit<'_> {
    fn deref_mut(&mut self) -> &mut Vec<Member> {
        &mut self.0.list
    }
}

impl Drop for MembersEdit<'_> {
    fn drop(&mut self) {
        self.0.reindex();
    }
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
    /// Their profile bio as they typed it, `:shortcodes:` included.
    pub bio: Option<String>,
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

/// A decoded profile picture, server icon, emoji or sticker: each frame and how long it
/// shows, in ms. An animated GIF loads with only its first frame; the others are decoded
/// on the shared picture workers the first time it plays (see `Picture::rest`).
#[derive(Clone)]
pub struct Picture {
    /// A hash of the image file, to keep its textures.
    pub key: [u8; 32],
    pub first: Arc<(ColorImage, u32)>,
    /// The frames after the first, shared by every clone.
    rest: Arc<Mutex<Rest>>,
    /// The image file as received, so an emoji or sticker can be sent to another server.
    pub bytes: Arc<Vec<u8>>,
}

enum Rest {
    /// Decoded up front and kept: what the textures are made from again.
    Kept(Vec<(ColorImage, u32)>),
    /// A lazy GIF not (or no longer) decoded.
    Idle,
    Decoding,
    /// Decoded, until the textures are made from them.
    Ready(Vec<(ColorImage, u32)>),
    /// A GIF that failed to decode in full: it stays on its first frame.
    Failed,
}

type Job = Box<dyn FnOnce() + Send>;

/// Threads decoding animated pictures, however many play at once.
pub(crate) const PICTURE_WORKERS: usize = 2;

/// The decode queue: each picture waits in it at most once, and a full queue only
/// delays a picture to a later repaint.
static DECODER: LazyLock<mpsc::SyncSender<Job>> = LazyLock::new(|| {
    let (sender, jobs) = mpsc::sync_channel::<Job>(64);
    let jobs = Arc::new(Mutex::new(jobs));
    for _ in 0..PICTURE_WORKERS {
        let jobs = jobs.clone();
        let spawned = std::thread::Builder::new()
            .name("picture frames".into())
            .spawn(move || {
                loop {
                    let job = jobs.lock().unwrap_or_else(PoisonError::into_inner).recv();
                    match job {
                        Ok(job) => job(),
                        Err(_) => break,
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start a picture decoding thread");
        }
    }
    sender
});

impl Picture {
    /// A picture with all its frames decoded already.
    pub fn new(key: [u8; 32], mut frames: Vec<(ColorImage, u32)>, bytes: Arc<Vec<u8>>) -> Self {
        let first = frames.remove(0);
        Self {
            key,
            first: Arc::new(first),
            rest: Arc::new(Mutex::new(Rest::Kept(frames))),
            bytes,
        }
    }

    /// An animated GIF of which only `first` is decoded; `rest` decodes the others.
    pub fn lazy(key: [u8; 32], first: (ColorImage, u32), bytes: Arc<Vec<u8>>) -> Self {
        Self {
            key,
            first: Arc::new(first),
            rest: Arc::new(Mutex::new(Rest::Idle)),
            bytes,
        }
    }

    /// The frames after the first, to make textures of, or `None` while they are
    /// decoded. The first call queues decoding them, which calls `done` when they are
    /// ready; later calls only wait for that one decode. A lazy GIF's frames are handed
    /// over and not kept, so asking again decodes them again. A file that fails to
    /// decode in full stays on its first frame.
    pub fn rest(&self, done: impl FnOnce() + Send + 'static) -> Option<Vec<(ColorImage, u32)>> {
        let mut rest = self.rest.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *rest {
            Rest::Kept(frames) => return Some(frames.clone()),
            Rest::Failed => return Some(Vec::new()),
            Rest::Decoding => return None,
            Rest::Ready(frames) => {
                let frames = std::mem::take(frames);
                *rest = Rest::Idle;
                return Some(frames);
            }
            Rest::Idle => {}
        }
        let (shared, bytes) = (self.rest.clone(), self.bytes.clone());
        let job = Box::new(move || {
            let decoded = match rusttalk_core::client::decode_profile_image(&bytes) {
                Ok(frames) => Rest::Ready(frames.into_iter().skip(1).map(frame_image).collect()),
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "animated picture could not be decoded");
                    Rest::Failed
                }
            };
            *shared.lock().unwrap_or_else(PoisonError::into_inner) = decoded;
            done();
        });
        *rest = match DECODER.try_send(job) {
            Ok(()) => Rest::Decoding,
            // Tried again at a later repaint; every decode that ends asks for one.
            Err(TrySendError::Full(_)) => Rest::Idle,
            Err(TrySendError::Disconnected(_)) => {
                tracing::warn!("no thread to decode an animated picture");
                Rest::Failed
            }
        };
        None
    }

    /// The bytes of the frames held in memory: the first, and any decoded but not yet
    /// handed over or kept.
    pub fn held_bytes(&self) -> usize {
        let rest = self.rest.lock().unwrap_or_else(PoisonError::into_inner);
        let rest = match &*rest {
            Rest::Kept(frames) | Rest::Ready(frames) => frames.as_slice(),
            _ => &[],
        };
        std::iter::once(&*self.first)
            .chain(rest)
            .map(|(image, _)| image.pixels.len() * 4)
            .sum()
    }
}

#[derive(Clone)]
pub struct Device {
    pub signing_key: Id,
    pub name: String,
    pub fingerprint: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Dm {
    pub channel: Id,
    pub peer: Id,
}

#[derive(Clone)]
pub struct Msg {
    pub backfilled: bool,
    pub id: Id,
    pub person: Id,
    pub at: i64,
    pub reply_to: Option<Id>,
    pub quoted_author: Option<Id>,
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
    /// A request to join a voice channel or its answer; rings, not shown in the chat.
    VoiceRequest(rusttalk_core::messages::VoiceRequest),
    /// A poll and its question, shown as a card.
    Poll(String, Poll),
    /// A vote or a poll's end; counted on the poll's card, not shown in the chat.
    PollPart(PollPart),
    /// Could not be verified or decrypted (yet).
    Unreadable,
    /// A membership change, with a fingerprint for the details view.
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

/// One of my messages on its way: shown right away, until the server's record of it
/// (with the same ID) arrives, or marked failed so it can be sent again.
pub struct Outgoing {
    pub person: Id,
    pub draft: crate::net::Draft,
    pub failed: bool,
}

impl Outgoing {
    /// How it shows until it is sent: as a message of mine, without a server time.
    pub fn msg(&self) -> Msg {
        Msg {
            backfilled: false,
            id: self.draft.id,
            person: self.person,
            at: 0,
            reply_to: self.draft.reply_to.map(|(_, id)| id),
            quoted_author: self.draft.reply_to.map(|(person, _)| person),
            edit_of: None,
            body: Body::Text(self.draft.text.clone()),
            files: Vec::new(),
            live: None,
            sticker: self.draft.sticker,
            replicas: Vec::new(),
            copy_files: Vec::new(),
        }
    }
}

/// One channel's loaded history, oldest first, plus the composer state.
#[derive(Default)]
pub struct Chat {
    pub new_post: crate::home::forum::NewPost,
    pub submissions: Vec<crate::home::forum::Submission>,
    pub messages: Vec<Msg>,
    /// My messages shown before the server confirmed them, oldest first.
    pub outbox: Vec<Outgoing>,
    /// Originals that show edited text, by person and ID, with the applied edit's rank.
    pub edited: HashMap<(Id, Id), Rank>,
    /// Edits whose original is not loaded yet, by person and original ID.
    pending_edits: HashMap<(Id, Id), Msg>,
    /// Votes and ends by poll ID, kept out of `messages` so they never show as lines.
    polls: HashMap<Id, PollEvents>,
    /// Reactions by the first half of their message's ID; they travel as votes.
    reactions: HashMap<[u8; 16], Vec<Reaction>>,
    /// The poll being written in place of the composer.
    pub poll_editor: Option<crate::home::polls::Editor>,
    /// The channel view has requested its initial history and live updates.
    pub opened: bool,
    pub loading: bool,
    pub at_start: bool,
    /// Oldest wire record, including votes and non-displayable records.
    pub oldest: Option<Id>,
    pub draft: String,
    pub mentions: crate::home::mentions::DraftMentions,
    /// The message replied to: person and ID.
    pub reply_to: Option<(Id, Id)>,
    /// One of this person's own messages.
    pub editing: Option<Id>,
    /// The message the emoji picker is open to react to, instead of typing.
    pub reacting: Option<Id>,
    /// Files picked or dropped, sent with the next message.
    pub staged: Vec<Staged>,
    /// Emoji of other servers picked for the draft; they go along as small files.
    pub foreign: Vec<CustomEmoji>,
    /// What is uploading now and how far (0 to 1), shown above the composer.
    pub uploading: Option<(String, f32)>,
    /// The channel's shared files, newest first; `None` until loaded.
    pub shared: Option<Vec<rusttalk_core::client::SharedFile>>,
    /// Whether the shared files were asked for since the files view opened.
    pub shared_asked: bool,
    /// Content height and vertical scroll offset last frame, and height before older
    /// messages were added, so paging waits until that page is laid out.
    pub content_height: f32,
    pub scroll_offset: f32,
    pub keep_scroll: Option<f32>,
    /// A scroll offset to go to next frame, as if scrolled there by hand.
    pub scroll_to: Option<f32>,
    /// Row heights and the row in view, so only rows near the view are laid out.
    pub rows: crate::home::rows::Rows,
    /// Where each ID is in `messages`, for reply quotes; rebuilt after changes.
    by_id: HashMap<Id, Vec<usize>>,
    by_id_len: Option<usize>,
}

impl Chat {
    pub fn clear_history(&mut self) {
        self.by_id_len = None;
        self.messages.clear();
        self.edited.clear();
        self.pending_edits.clear();
        self.polls.clear();
        self.reactions.clear();
        self.at_start = false;
        self.oldest = None;
    }

    /// Show a new message of mine at once. Edits, polls, pokes, friend and voice
    /// requests and forum posts have their own ways of showing they're on the way.
    pub fn outgoing(&mut self, person: Id, draft: &crate::net::Draft) {
        let plain = draft.edit_of.is_none()
            && !draft.forum_post
            && !draft.poke
            && draft.poll.is_none()
            && draft.friend.is_none()
            && draft.voice_request.is_none();
        if plain && !self.outbox.iter().any(|out| out.draft.id == draft.id) {
            self.outbox.push(Outgoing {
                person,
                draft: draft.clone(),
                failed: false,
            });
        }
    }

    /// Sending failed: mark it, keeping what was made for it (a link preview) for a
    /// retry. Returns the draft when it wasn't shown here.
    pub fn unsent(&mut self, draft: crate::net::Draft) -> Option<crate::net::Draft> {
        let Some(out) = (self.outbox.iter_mut()).find(|out| out.draft.id == draft.id) else {
            return Some(draft);
        };
        out.failed = true;
        out.draft = draft;
        None
    }

    /// The same message again, with the same ID, so it can't arrive twice.
    pub fn retry(&mut self, id: &Id) -> Option<crate::net::Draft> {
        let out = (self.outbox.iter_mut()).find(|out| out.draft.id == *id && out.failed)?;
        out.failed = false;
        Some(out.draft.clone())
    }

    /// Give up on a failed message.
    pub fn discard(&mut self, id: &Id) -> Option<crate::net::Draft> {
        let at = (self.outbox.iter()).position(|out| out.draft.id == *id && out.failed)?;
        Some(self.outbox.remove(at).draft)
    }

    pub fn open_draft(&mut self, dir: &Directory, text: &str) {
        (self.draft, self.mentions) = crate::home::mentions::DraftMentions::open(dir, text);
    }

    pub fn clear_draft(&mut self) {
        self.draft.clear();
        self.foreign.clear();
        self.mentions = Default::default();
    }

    /// Add records from a page or a live event, applying edits to their originals.
    pub fn merge(&mut self, items: Vec<Msg>, older: bool) {
        // A live message that belongs at the end: skip the dedup and the sort. The
        // list is always sorted here, since every other path through merge sorts it.
        if let [msg] = items.as_slice()
            && !older
            && msg.edit_of.is_none()
            && !matches!(msg.body, Body::PollPart(_))
            && self.messages.last().is_none_or(|last| last.at <= msg.at)
            && !(self.messages.iter().rev()).any(|old| old.id == msg.id && old.person == msg.person)
        {
            // At the end, the index stays right with one more entry.
            if self.by_id_len == Some(self.messages.len()) {
                self.by_id
                    .entry(msg.id)
                    .or_default()
                    .push(self.messages.len());
                self.by_id_len = Some(self.messages.len() + 1);
            }
            self.messages.extend(items);
            self.settle(Vec::new());
            return;
        }
        self.by_id_len = None;
        self.merge_all(items, older);
    }

    /// `merge` for any batch: deduplicate, sort, then settle.
    fn merge_all(&mut self, items: Vec<Msg>, older: bool) {
        let (parts, items): (Vec<_>, Vec<_>) = (items.into_iter())
            .partition(|msg| matches!(msg.body, Body::PollPart(_)) && msg.edit_of.is_none());
        // A retried vote replaces its "couldn't be opened" placeholder.
        self.messages.retain(|msg| {
            msg.body != Body::Unreadable
                || !(parts.iter()).any(|part| part.id == msg.id && part.person == msg.person)
        });
        for msg in parts {
            let Body::PollPart(part) = msg.body else {
                continue;
            };
            let reaction = Reaction::from_vote(&part, msg.person, msg.at, msg.id);
            let (poll, event) = match part {
                PollPart::Vote {
                    poll,
                    sent,
                    choices,
                } => (poll, Ok((sent, choices))),
                PollPart::End { poll } => (poll, Err(())),
                PollPart::Open(_) => continue,
            };
            let events = self.polls.entry(poll).or_default();
            // DM copies from several servers count once.
            if events.seen.contains(&(msg.person, msg.id)) {
                continue;
            }
            events.seen.push((msg.person, msg.id));
            if let Some((target, reaction)) = reaction {
                self.reactions.entry(target).or_default().push(reaction);
            }
            match event {
                Ok((sent, choices)) => events.ballots.push(Ballot {
                    person: msg.person,
                    at: msg.at,
                    sent,
                    id: msg.id,
                    choices,
                }),
                Err(()) => events.ends.push((msg.person, msg.at)),
            }
        }
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
                    // A protocol 1 replica can arrive before the same v3 reply.
                    if old.reply_to == msg.reply_to && old.quoted_author.is_none() {
                        old.quoted_author = msg.quoted_author;
                    }
                    if (msg.backfilled, msg.at) < (old.backfilled, old.at) {
                        old.at = msg.at;
                        old.backfilled = msg.backfilled;
                    }
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
        self.settle(edits);
    }

    /// After new records are in place: drop what they replace and apply edits.
    fn settle(&mut self, edits: Vec<Msg>) {
        // The server's record replaces the one shown while sending, whether it came
        // with the send's answer or as a live event first.
        let messages = &self.messages;
        self.outbox.retain(|out| {
            !(messages.iter()).any(|msg| msg.id == out.draft.id && msg.person == out.person)
        });
        // A retried edit replaces its "couldn't be opened" placeholder.
        if !edits.is_empty() {
            self.by_id_len = None;
        }
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

    /// A poll message's results so far.
    pub fn tally(&self, msg: &Msg) -> Option<Tally> {
        let Body::Poll(_, poll) = &msg.body else {
            return None;
        };
        let events = self.polls.get(&msg.id);
        let ballots = events.map_or(&[][..], |events| &events.ballots);
        let ends = events.map_or(&[][..], |events| &events.ends);
        Some(rusttalk_core::messages::tally(
            poll,
            &msg.person,
            ballots,
            ends,
        ))
    }

    /// The up (option 0) and down (option 1) votes on a forum post. They are sent as
    /// votes on the post itself, so they can be changed or taken back like poll votes,
    /// and apps without forums leave them out.
    pub fn post_votes(&self, msg: &Msg) -> Tally {
        let both = rusttalk_core::messages::Poll {
            options: vec![String::new(), String::new()],
            multiple: false,
            ends_at: 0,
        };
        let events = self.polls.get(&msg.id);
        let ballots = events.map_or(&[][..], |events| &events.ballots);
        rusttalk_core::messages::tally(&both, &msg.person, ballots, &[])
    }

    /// A message's reactions: each emoji (by key) and who reacted with it, for the
    /// emoji `known` says this app can show.
    pub fn reactions(
        &self,
        msg: &Id,
        known: impl Fn(&[u8; 16]) -> bool,
    ) -> Vec<([u8; 16], Vec<Id>)> {
        let half: [u8; 16] = msg[..16].try_into().expect("16 bytes");
        self.reactions
            .get(&half)
            .map_or_else(Vec::new, |reactions| {
                rusttalk_core::messages::tally_reactions(reactions, known)
            })
    }

    /// A verified sender wins over the reply's display hint. For colliding IDs,
    /// only the named person's verified message can supply a snippet.
    pub fn quoted(&self, id: &Id, author: Option<Id>) -> Option<&Msg> {
        let found: Vec<&Msg> = if self.by_id_len == Some(self.messages.len()) {
            (self.by_id.get(id).into_iter().flatten())
                .filter_map(|at| self.messages.get(*at))
                .filter(|msg| msg.id == *id)
                .collect()
        } else {
            self.messages.iter().filter(|msg| msg.id == *id).collect()
        };
        if let Some(person) = author
            && let Some(named) =
                (found.iter()).find(|msg| msg.person == person && matches!(msg.body, Body::Text(_)))
        {
            return Some(named);
        }
        let first = found.first()?;
        (matches!(first.body, Body::Text(_)) && found.iter().all(|msg| msg.person == first.person))
            .then_some(first)
    }

    /// Index the messages by ID for `quoted`, if they changed since the last time.
    pub fn index_ids(&mut self) {
        if self.by_id_len == Some(self.messages.len()) {
            return;
        }
        self.by_id.clear();
        for (at, msg) in self.messages.iter().enumerate() {
            self.by_id.entry(msg.id).or_default().push(at);
        }
        self.by_id_len = Some(self.messages.len());
    }

    /// Drop a deleted record. Edits of a deleted original stay hidden.
    // shortcut: deleting an edit record keeps its text on the original, until a reload.
    pub fn remove(&mut self, id: &Id) {
        self.by_id_len = None;
        self.messages.retain(|msg| msg.id != *id);
        self.pending_edits.retain(|(_, original), _| original != id);
        self.edited.retain(|(_, original), _| original != id);
    }

    /// Drop one person's record, leaving another person's under the same ID.
    pub fn remove_one(&mut self, person: &Id, id: &Id) {
        let key = (*person, *id);
        self.by_id_len = None;
        self.messages
            .retain(|msg| msg.id != *id || msg.person != *person);
        self.pending_edits.remove(&key);
        self.edited.remove(&key);
    }
}

/// One poll's votes and ends as received.
#[derive(Default)]
struct PollEvents {
    ballots: Vec<Ballot>,
    /// Who ended it and when, in server time.
    ends: Vec<(Id, i64)>,
    /// Person and record ID of each vote and end taken.
    seen: Vec<(Id, Id)>,
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
            bio: None,
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
            backfilled: false,
            id: [id; 32],
            person: [9; 32],
            at,
            reply_to: None,
            quoted_author: None,
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

    fn draft(id: u8, text: &str) -> crate::net::Draft {
        crate::net::Draft {
            forum_post: false,
            id: [id; 32],
            channel: [7; 32],
            peer: None,
            text: text.into(),
            reply_to: None,
            edit_of: None,
            revision: 0,
            files: Vec::new(),
            keep: Vec::new(),
            sticker: None,
            friend: None,
            poke: false,
            poll: None,
            voice_request: None,
            preview: None,
            inline: Vec::new(),
            queued: Default::default(),
        }
    }

    fn outbox(chat: &Chat) -> Vec<(u8, bool)> {
        (chat.outbox.iter())
            .map(|out| (out.draft.id[0], out.failed))
            .collect()
    }

    #[test]
    fn sent_message_replaces_its_pending_copy_once_in_either_order() {
        // The send's answer and the live event both carry the record, in either order.
        for older in [false, true] {
            let mut chat = Chat::default();
            chat.outgoing([9; 32], &draft(1, "hi"));
            chat.outgoing([9; 32], &draft(2, "there"));
            assert_eq!(outbox(&chat), [(1, false), (2, false)]);
            let confirmed = msg(1, 10, "hi", None);
            chat.merge(vec![confirmed.clone()], false);
            assert_eq!(outbox(&chat), [(2, false)]);
            // The second copy of the same record changes nothing.
            chat.merge(vec![confirmed], older);
            assert_eq!(texts(&chat), ["hi"]);
            assert_eq!(outbox(&chat), [(2, false)]);
        }
    }

    #[test]
    fn someone_elses_record_with_my_id_does_not_confirm_mine() {
        let mut chat = Chat::default();
        chat.outgoing([8; 32], &draft(1, "mine"));
        chat.merge(vec![msg(1, 10, "theirs", None)], false);
        assert_eq!(outbox(&chat), [(1, false)]);
    }

    #[test]
    fn failed_message_can_be_retried_or_discarded() {
        let mut chat = Chat::default();
        chat.outgoing([9; 32], &draft(1, "a"));
        chat.outgoing([9; 32], &draft(2, "b"));
        // Shown twice by mistake: still one entry.
        chat.outgoing([9; 32], &draft(1, "a"));
        assert!(
            chat.retry(&[1; 32]).is_none(),
            "only failed ones are retried"
        );
        assert!(chat.unsent(draft(1, "a")).is_none());
        assert!(chat.unsent(draft(2, "b")).is_none());
        assert_eq!(outbox(&chat), [(1, true), (2, true)]);
        // Not shown here (an edit, say): handed back for the composer.
        assert!(chat.unsent(draft(3, "c")).is_some());
        let again = chat.retry(&[1; 32]).expect("failed");
        assert_eq!((again.id, again.text.as_str()), ([1; 32], "a"));
        assert_eq!(outbox(&chat), [(1, false), (2, true)]);
        assert!(chat.discard(&[2; 32]).is_some());
        assert_eq!(outbox(&chat), [(1, false)]);
        // The retry went through.
        chat.merge(vec![msg(1, 10, "a", None)], false);
        assert!(chat.outbox.is_empty());
    }

    #[test]
    fn only_plain_new_messages_show_while_sending() {
        let mut chat = Chat::default();
        let mut edit = draft(1, "fixed");
        edit.edit_of = Some([5; 32]);
        let mut poke = draft(2, "hey");
        poke.poke = true;
        for skipped in [edit, poke] {
            chat.outgoing([9; 32], &skipped);
        }
        assert!(chat.outbox.is_empty());
    }

    #[test]
    fn normal_copy_sets_time_in_either_arrival_order() {
        let normal = msg(1, 100, "hello", None);
        let restored = Msg {
            backfilled: true,
            at: 5,
            ..normal.clone()
        };
        for pair in [[normal.clone(), restored.clone()], [restored, normal]] {
            let mut chat = Chat::default();
            chat.merge(pair.into(), false);
            assert_eq!(chat.messages.len(), 1);
            assert_eq!(chat.messages[0].at, 100);
            assert!(!chat.messages[0].backfilled);
        }
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
        assert_eq!(chat.quoted(&[1; 32], None).map(|m| m.person), Some([9; 32]));
        assert_eq!(
            chat.quoted(&[1; 32], Some([8; 32])).unwrap().person,
            [9; 32]
        );
        let mut reply = msg(7, 20, "reply", None);
        reply.reply_to = Some([1; 32]);
        let mut replicas = Chat::default();
        replicas.merge(vec![reply.clone()], false);
        reply.quoted_author = Some([9; 32]);
        replicas.merge(vec![reply], false);
        assert_eq!(replicas.messages[0].quoted_author, Some([9; 32]));
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
        assert!(chat.quoted(&[1; 32], None).is_none());
        assert!(chat.quoted(&[5; 32], None).is_none());
        assert_eq!(
            chat.quoted(&[1; 32], Some([8; 32])).unwrap().person,
            [8; 32]
        );
        // My own edit still applies to mine, and removing theirs leaves mine.
        chat.merge(vec![revised(3, 13, "mine2", 1, 1)], false);
        chat.remove_one(&[8; 32], &[1; 32]);
        assert_eq!(texts(&chat), ["mine2"]);
        assert!(chat.edited.contains_key(&([9; 32], [1; 32])));
    }

    /// The ID index for quotes answers like a scan, however the list changed since.
    #[test]
    fn quotes_found_by_index_match_a_scan_after_every_change() {
        let mut chat = Chat::default();
        let check = |chat: &mut Chat| {
            chat.index_ids();
            for id in 0..=60 {
                let indexed = chat.quoted(&[id; 32], None).map(|m| (m.id, m.at));
                let scanned = (chat.messages.iter()).find(|m| m.id == [id; 32]);
                assert_eq!(indexed, scanned.map(|m| (m.id, m.at)), "message {id}");
            }
        };
        chat.merge(
            (30..40).map(|i| msg(i, i64::from(i), "x", None)).collect(),
            false,
        );
        check(&mut chat);
        // Live messages one by one keep the index without a rebuild.
        for i in 40..45 {
            chat.merge(vec![msg(i, i64::from(i), "x", None)], false);
            assert!(chat.by_id_len.is_some());
            check(&mut chat);
        }
        // An older page shifts every position.
        chat.merge(
            (1..30).map(|i| msg(i, i64::from(i), "x", None)).collect(),
            true,
        );
        check(&mut chat);
        chat.remove(&[5; 32]);
        check(&mut chat);
        // Changed by hand: the length differs, so it scans until indexed again.
        chat.messages.retain(|m| m.id != [6; 32]);
        assert!(chat.quoted(&[6; 32], None).is_none());
        assert!(chat.quoted(&[7; 32], None).is_some());
        check(&mut chat);
    }

    #[test]
    fn votes_stay_out_of_the_chat_and_count_on_their_poll() {
        let poll = Msg {
            body: Body::Poll(
                "Lunch?".into(),
                Poll {
                    options: vec!["Pizza".into(), "Sushi".into()],
                    multiple: false,
                    ends_at: 0,
                },
            ),
            ..msg(1, 10, "", None)
        };
        let vote = |id: u8, person: u8, sent: u64, choice: u8| Msg {
            person: [person; 32],
            body: Body::PollPart(PollPart::Vote {
                poll: [1; 32],
                sent,
                choices: vec![choice],
            }),
            ..msg(id, 20, "", None)
        };
        let mut chat = Chat::default();
        // A vote can arrive before its poll is loaded.
        chat.merge(vec![vote(2, 5, 1, 0)], false);
        let unreadable = Msg {
            body: Body::Unreadable,
            ..vote(3, 6, 1, 1)
        };
        chat.merge(vec![poll.clone(), unreadable], true);
        // The retry replaces its placeholder; a second DM copy counts once.
        chat.merge(vec![vote(3, 6, 1, 1), vote(3, 6, 1, 1)], false);
        assert_eq!(chat.messages.len(), 1);
        let tally = chat.tally(&chat.messages[0]).unwrap();
        assert_eq!(tally.voters, [vec![[5; 32]], vec![[6; 32]]]);
        // Only the poll's creator ends it.
        let end = |person: u8| Msg {
            person: [person; 32],
            body: Body::PollPart(PollPart::End { poll: [1; 32] }),
            ..msg(4, 30, "", None)
        };
        chat.merge(vec![end(5)], false);
        assert!(!chat.tally(&poll).unwrap().ended);
        chat.merge(
            vec![Msg {
                id: [5; 32],
                ..end(9)
            }],
            false,
        );
        assert!(chat.tally(&poll).unwrap().ended);
        assert_eq!(chat.messages.len(), 1);
    }

    fn person(id: u8, name: &str) -> Member {
        Member {
            person: [id; 32],
            name: Some(name.into()),
            devices: Vec::new(),
            roles: Vec::new(),
            avatar: None,
            presence: Presence::Online,
            hide_offline: false,
            bio: None,
            voice_muted: false,
        }
    }

    /// Every lookup, including people not listed, finds what a scan finds.
    fn assert_index_matches_scan(members: &Members) {
        for id in 0..8 {
            let scanned = members.iter().find(|m| m.person == [id; 32]);
            let indexed = members.get(&[id; 32]);
            assert_eq!(
                scanned.map(|m| m as *const Member),
                indexed.map(|m| m as *const Member),
                "person {id}"
            );
        }
    }

    #[test]
    fn member_index_matches_a_scan_after_changes() {
        // A duplicate row: the first one wins, as with a scan.
        let mut members: Members = vec![person(1, "a"), person(2, "b"), person(1, "dup")].into();
        assert_index_matches_scan(&members);
        assert_eq!(members.get(&[1; 32]).unwrap().display_name(), "a");
        members.edit().push(person(3, "c"));
        assert_index_matches_scan(&members);
        members
            .edit()
            .retain(|m| m.person != [1; 32] || m.name.as_deref() == Some("dup"));
        assert_index_matches_scan(&members);
        assert_eq!(members.get(&[1; 32]).unwrap().display_name(), "dup");
        members.edit()[0].person = [4; 32];
        assert_index_matches_scan(&members);
        assert!(members.get(&[2; 32]).is_none());
        members.edit().insert(0, person(5, "e"));
        assert_index_matches_scan(&members);
        members = [person(6, "f")].into_iter().collect();
        assert_index_matches_scan(&members);
        let copy = members.clone();
        assert_index_matches_scan(&copy);
        assert_index_matches_scan(&Members::default());
    }

    #[test]
    fn member_lookups_use_the_index() {
        let dir = Directory {
            members: vec![person(1, "Ann"), person(2, "Bob")].into(),
            ..Directory::default()
        };
        assert_eq!(member_name(&dir, &[2; 32]), "Bob");
        assert!(member_name(&dir, &[3; 32]).contains(&fingerprint(&[3; 32])[..4]));
    }

    /// What a chat shows and keeps, to compare two of them.
    fn snapshot(chat: &Chat) -> impl PartialEq + std::fmt::Debug {
        let messages: Vec<_> = (chat.messages.iter())
            .map(|msg| {
                let body = match &msg.body {
                    Body::Text(text) => text.clone(),
                    Body::Unreadable => "unreadable".into(),
                    _ => "?".into(),
                };
                let replicas: Vec<_> = msg.replicas.iter().map(Url::to_string).collect();
                (
                    msg.id[0],
                    msg.person[0],
                    msg.at,
                    msg.backfilled,
                    body,
                    replicas,
                )
            })
            .collect();
        let outbox: Vec<_> = chat.outbox.iter().map(|out| out.draft.id[0]).collect();
        let mut edited: Vec<_> = (chat.edited.iter())
            .map(|(key, rank)| (key.0[0], key.1[0], *rank))
            .collect();
        edited.sort();
        let mut pending: Vec<_> = chat
            .pending_edits
            .keys()
            .map(|k| (k.0[0], k.1[0]))
            .collect();
        pending.sort();
        (messages, outbox, edited, pending)
    }

    #[test]
    fn live_append_fast_path_matches_the_full_merge() {
        let other = |id: u8, at: i64, text: &str| Msg {
            person: [8; 32],
            ..msg(id, at, text, None)
        };
        let unreadable = |id: u8, at: i64| Msg {
            body: Body::Unreadable,
            ..msg(id, at, "", None)
        };
        let copy = |id: u8, at: i64, text: &str, on: u8| Msg {
            replicas: vec![server(on)],
            ..msg(id, at, text, None)
        };
        let steps = [
            // In order, including the same second and an edit of a loaded message.
            msg(1, 10, "a", None),
            msg(2, 20, "b", None),
            msg(3, 20, "c", None),
            msg(10, 21, "b2", Some(2)),
            // An edit whose original comes later, then the original, in order.
            msg(11, 22, "d2", Some(4)),
            msg(4, 23, "d", None),
            // Out of order.
            msg(5, 15, "e", None),
            // Duplicates: same time, a later time, and a later DM copy elsewhere.
            msg(4, 23, "d", None),
            msg(2, 30, "b", None),
            copy(3, 31, "c", 2),
            // Another person's record with an ID of mine is a separate message.
            other(1, 32, "theirs"),
            // A placeholder, then its readable retry later on.
            unreadable(6, 33),
            msg(6, 34, "f", None),
            // My pending message, confirmed once, then again.
            msg(7, 35, "sent", None),
            msg(7, 36, "sent", None),
            msg(8, 37, "later", None),
        ];
        let (mut fast, mut full) = (Chat::default(), Chat::default());
        for chat in [&mut fast, &mut full] {
            chat.outgoing([9; 32], &draft(7, "sent"));
            chat.outgoing([9; 32], &draft(8, "later"));
        }
        for (step, item) in steps.into_iter().enumerate() {
            fast.merge(vec![item.clone()], false);
            full.merge_all(vec![item], false);
            assert_eq!(snapshot(&fast), snapshot(&full), "after step {step}");
        }
        assert_eq!(
            texts(&fast),
            ["a", "e", "b2", "c", "d2", "theirs", "f", "sent", "later"]
        );
        assert!(fast.outbox.is_empty());
        // Older pages never take the fast path, also when a record sorts last.
        for chat in [&mut fast, &mut full] {
            chat.merge(vec![msg(9, 40, "old", None)], true);
        }
        assert_eq!(snapshot(&fast), snapshot(&full));
    }
}
