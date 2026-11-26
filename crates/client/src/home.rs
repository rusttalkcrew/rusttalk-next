//! The signed-in window: server rail, channels, chat and members.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use chrono::{DateTime, Local};
use eframe::egui::{
    self, Align, Align2, CentralPanel, CollapsingHeader, Color32, ColorImage, DragValue, FontId,
    Frame, Id as UiId, Key, KeyboardShortcut, Label, LayerId, Layout, Margin, Modifiers, Order,
    Panel, Rect, Response, RichText, Role, ScrollArea, Sense, Shape, Stroke, StrokeKind, TextEdit,
    TextureHandle, TextureOptions, Ui, UiBuilder, Vec2, WidgetInfo, ecolor::Hsva,
};
use rusttalk_core::{
    auth::{Fingerprint, InviteLink, KnownServer, decode},
    client::{Attachment, FriendSignal, MAX_TEXT_CHARS, voice::LiveFileEvent},
    identity::{DeviceIdentity, fingerprint},
    index::https_url,
    messages::{PollPart, reaction_vote},
    presence::{Presence, Status as Choice},
    terminal::Feed,
};

use crate::{
    admin,
    app::{Action, submitted},
    audio::Audio,
    call::{self, LiveAction, LiveFile, LiveState, Voice},
    crop::{Crop, Outcome, Target},
    device_sync::{self, Edit},
    dm::{self, Thread},
    emoji::{
        self,
        picker::{self, Picked, Picker, Tab as PickerTab},
    },
    files::{self, Ask, Files},
    friends::{Act, Friends, Relation},
    link_cards,
    net::{
        Admin, AdminUpdate, Admission, Command, Compatibility, Draft, Listed, Net, PAGE_SIZE,
        Preview, Update,
    },
    notify::{self, Arrival, Notify},
    official,
    previews::{self, Previews},
    reorder,
    screen::Video,
    server::{
        Body, Channel, ChannelKind, Chat, CustomEmoji, Directory, Id, Member, Msg, Picture, Server,
        Status, member_name,
    },
    sticker_files::{self, Inline, Kind},
    ui::{self, Item, Look, Palette, error_text},
};

/// Text fields in settings rows.
const FIELD_WIDTH: f32 = 280.0;
/// Show the character count once a message gets this long.
const COUNT_FROM: usize = 1_800;
/// The buttons in the message bar.
const COMPOSER_BUTTON: f32 = ui::ICON_BUTTON + 4.0;
/// Above and below a line of text in the message bar, centring it on the buttons
/// (a line of body text is about 18 points tall).
const COMPOSER_TEXT_PAD: i8 = ((COMPOSER_BUTTON - 18.0) / 2.0) as i8;
/// Around the buttons inside the message bar.
const COMPOSER_PADDING: f32 = ui::SPACE_XS + 2.0;
/// The message bar's height with one line of text. The user bar in the bottom left
/// is as tall plus its frame, so both share one centre line.
pub const COMPOSER_HEIGHT: f32 = COMPOSER_BUTTON + 2.0 * COMPOSER_PADDING;
const DAY: u32 = 24 * 60 * 60;
/// Seconds without any input before "Online" turns into "Away" by itself.
const AWAY_AFTER: f64 = 10.0 * 60.0;
const RAIL_WIDTH: f32 = 72.0;
const RAIL_BUTTON: f32 = 48.0;
const SERVER_HEADER: f32 = 44.0;
const MESSAGE_AVATAR: f32 = 38.0;
/// The replied-to person's picture on a reply line.
const REPLY_AVATAR: f32 = 16.0;
/// How long a message jumped to from a reply stays highlighted, fading out.
const REPLY_FLASH_SECS: f64 = 1.5;
/// Older pages (of `net::PAGE` messages) to look through for a replied-to message.
const REPLY_SEEK_PAGES: u8 = 20;
pub(crate) const STICKER_SIZE: f32 = 160.0;
/// Emoji of other servers one message may carry as files: a file stays free for a
/// picture or a link preview.
const MAX_FOREIGN: usize = 3;

/// Whether this device counts as idle: no input in the app or a call (`active`) and
/// none anywhere on the computer for a while. `in_app` is the seconds since the last
/// input in the app, `system` the seconds since the last input anywhere when the
/// system tells; without it only the app's input counts.
fn device_idle(active: bool, in_app: f64, system: Option<f64>) -> bool {
    !active && system.map_or(in_app, |idle| idle.min(in_app)) > AWAY_AFTER
}

/// What the cards under messages need: joined servers for invites, and link previews.
struct Embeds<'a> {
    servers: &'a [Server],
    previews: &'a Previews,
    allowed: &'a BTreeMap<String, bool>,
}
const TOOLBAR_BUTTON: Vec2 = Vec2::new(30.0, 26.0);

pub(crate) mod embed;
pub(crate) mod events;
pub(crate) mod focus;
pub(crate) mod forum;
mod friends;
mod gallery;
pub mod layout;
mod markdown;
pub mod mentions;
pub mod person;
mod pokes;
pub mod polls;
pub(crate) mod rows;
mod switcher;
pub(crate) mod user_bar;
pub(crate) mod view_as;
pub mod voice_requests;
#[cfg(test)]
mod voice_rows;
mod welcome;

pub struct Home {
    pub identity: Arc<DeviceIdentity>,
    net: Net,
    pub servers: Vec<Server>,
    /// DMs by the other person, merged over every server they were on.
    threads: HashMap<Id, Thread>,
    backfill: Option<tokio::task::JoinHandle<()>>,
    next_backfill: f64,
    /// Friends, requests and blocks; synced between this person's devices only.
    pub(crate) friends: Friends,
    friends_tab: friends::Tab,
    /// Pokes received and sent; kept on this device only.
    pokes: crate::pokes::Pokes,
    /// The pop-up for the newest poke.
    popup: Option<pokes::Popup>,
    /// Requests to join a voice channel that ring here, and this person's own.
    pub(crate) requests: crate::voice_requests::Requests,
    /// This person's DM messages they deleted, on any device. All of them stay in
    /// `dm_deleted.json` here; the device sync copy holds the newest that fit.
    dm_deleted: dm::Tombstones,
    dm_deleted_path: PathBuf,
    view: View,
    invite: String,
    join: Join,
    find: Find,
    dialog: Option<Dialog>,
    focus: Option<focus::Dialog>,
    pub toast: Option<(String, f64)>,
    /// Android: the note that messages were left unsent, pending until its own
    /// notice was drawn (other toasts don't count).
    #[cfg(target_os = "android")]
    unsent: Option<PathBuf>,
    pub settings: Option<Tab>,
    pub voice: Voice,
    pub hotkeys: crate::hotkeys::Hotkeys,
    pub files: Files,
    /// Link previews fetched on this device.
    pub previews: Previews,
    /// Messages waiting for their link preview, and any sent after them to the same
    /// chat, so they still go out in order.
    sending: VecDeque<Outgoing>,
    emoji_picker: Picker,
    /// The quick switcher (Ctrl+K).
    switcher: switcher::Switcher,
    pub confirm_sign_out: bool,
    pub remove_device: Option<crate::settings::RemoveDevice>,
    pub error: Option<String>,
    pub key_storage: crate::key_storage::Form,
    pub route_editor: crate::route::RouteEditor,
    pub route_generation: u64,
    pub server_settings: Option<admin::Settings>,
    pub kick: Option<admin::Kick>,
    /// The crop step for a picked profile picture or server icon.
    pub(crate) crop: Option<crate::crop::Crop>,
    /// My account: the default name being edited, and the bio as typed, unsaved.
    pub name_draft: Option<String>,
    pub bio_draft: Option<String>,
    /// "What should people call you?" and the bio after it (`welcome`).
    welcome: welcome::Step,
    /// Made on this device just now: asks for a name at once.
    pub new_account: bool,
    /// A device sync has delivered the synced account data at least once.
    synced_once: bool,
    /// The synced voice and video settings as last published or received.
    synced: BTreeMap<String, serde_json::Value>,
    /// Servers left in this session, so a sync that started before can't bring them back.
    left: HashSet<url::Url>,
    /// This person's status on every server; `None` until a server reports it.
    pub status: Option<Choice>,
    /// No input or call on this device for a while. Servers show an "Online" choice as
    /// away only while all my devices are idle.
    idle: bool,
    /// Servers that were offline when I last picked a status: they get it once back.
    status_unsent: HashSet<url::Url>,
    /// When the person last used the window, in egui time.
    active_at: f64,
    /// How long the whole computer has been without input, so using other apps counts.
    system_idle: crate::idle::SystemIdle,
    pub layout: layout::Layout,
    pub notify: Notify,
    /// The window has focus: the open channel counts as read.
    focused: bool,
    /// Set up from the recovery words in this session: the join page helps find the servers.
    pub restored: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Join,
    /// The public server directory.
    Find,
    /// A DM, by the other person.
    Direct(Option<Id>),
    /// Pokes received and sent.
    Pokes,
    Server(usize),
}

/// The t!("home.find_servers") page.
#[derive(Default)]
struct Find {
    query: String,
    results: Vec<Listed>,
    /// The last page loaded, and whether there may be more.
    page: u32,
    more: bool,
    searching: bool,
    /// Checking a picked server.
    checking: bool,
    error: Option<String>,
    /// Just opened: list servers without waiting for a click.
    opened: bool,
}

impl Find {
    fn open(&mut self) {
        self.opened = true;
    }

    /// The page to load on its own: the first, once per opening.
    fn auto_page(&mut self) -> Option<u32> {
        std::mem::take(&mut self.opened).then_some(1)
    }
}

enum Join {
    Idle(Option<String>),
    UpdateRequired,
    Checking,
    Preview { preview: Box<Preview>, name: String },
    Joining(url::Url),
}

/// Voice channel bitrates a manager can pick, in kbps.
const AUDIO_KBPS: std::ops::RangeInclusive<u32> = 8..=rusttalk_core::client::voice::MAX_AUDIO_KBPS;
/// Voice channel member limits a manager can pick; 0 is no limit.
const MEMBER_LIMIT: std::ops::RangeInclusive<u32> = 0..=99;

/// A member limit typed into its field. An emptied field (or "No limit") means no
/// limit, so clearing it removes the limit instead of quietly keeping the old one.
fn no_limit_parser(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case(t!("home.no_limit")) {
        return Some(0.0);
    }
    text.parse::<u32>().ok().map(f64::from)
}

/// The height of both Edit channel tabs; the list of roles scrolls inside it.
const CHANNEL_TAB_HEIGHT: f32 = 350.0;

enum Dialog {
    ServerName(usize, String),
    MyName(usize, String),
    Category(usize, String),
    Channel {
        server: usize,
        name: String,
        kind: ChannelKind,
        category: Option<Id>,
        new_category: String,
    },
    EditChannel {
        server: usize,
        id: Id,
        tab: ChannelTab,
        name: String,
        topic: String,
        topic_changed: bool,
        /// Voice channels only: the bitrate in kbps, and whether it was moved.
        audio: Option<(u32, bool)>,
        /// Voice channels only: the member limit (0 for none), and whether it was changed.
        limit: Option<(u32, bool)>,
        /// Role flags changed on the Permissions tab, sent with Save.
        flags: Vec<(String, u8)>,
    },
    RenameCategory(usize, Id, String),
    DeleteItem(usize, reorder::Item, String),
    Invite(Invite),
    Member(usize, Id),
    Delete(usize, Id, Id),
    /// Remove a file from a channel's shared files: server, channel and the file.
    Unshare(usize, Id, Box<rusttalk_core::client::SharedFile>),
    /// Delete my DM message with this person, on every server holding it.
    DeleteDm(Id, Id),
    /// Leave a server, with the reason it didn't work.
    Leave(usize, Option<String>),
    /// Poke someone, with a short note.
    Poke(Id, String),
}

pub(crate) enum ChannelEdit {
    Rename,
    Delete,
}

#[derive(Clone, Copy, PartialEq)]
enum ChannelTab {
    General,
    Permissions,
}

struct Invite {
    server: usize,
    uses: u32,
    seconds: u32,
    /// Waiting for the link.
    making: bool,
    /// The invite's ID and link.
    link: Option<(String, String)>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tab {
    Profile,
    Appearance,
    Voice,
    Video,
    Privacy,
    Devices,
    Notifications,
    Shortcuts,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    Phone,
    Network,
    #[cfg(target_os = "android")]
    Advanced,
    Updates,
    About,
    WhatsNew,
}

impl Home {
    pub fn stop_for_sign_out(&mut self) {
        #[cfg(target_os = "android")]
        call::leave(self);
        self.net.stop_for_sign_out();
        self.voice.audio.stop_saving();
        self.notify.stop_saving();
    }

    /// Close active calls and refresh clients after switching the per-app network route.
    pub fn on_route_changed(&mut self, updates: &mut crate::update::Updater) {
        call::leave(self);
        self.previews.on_route_changed();
        updates.on_route_changed();
    }

    pub fn new(identity: DeviceIdentity, ctx: &egui::Context, data: PathBuf) -> Result<Self> {
        let identity = Arc::new(identity);
        let data_dir = data.clone();
        #[cfg(target_os = "android")]
        let unsent = crate::net::unsent_note(&data_dir);
        let fresh = !data.join("voice.json").exists();
        let audio = Audio::new(&data)?;
        audio.set_repaint(ctx.clone());
        // After an update, open "What's new" once; a first start has nothing new yet.
        let version = env!("CARGO_PKG_VERSION");
        let mut saved = audio.settings();
        let announce = crate::whats_new::announce(&saved.whats_new_seen, version, fresh);
        if saved.whats_new_seen != version {
            saved.whats_new_seen = version.into();
            audio.set_settings(saved);
        }
        let video = Video::new(ctx.clone());
        let net = Net::new(
            ctx.clone(),
            identity.clone(),
            data,
            audio.clone(),
            video.clone(),
        )?;
        let servers: Vec<_> = net
            .joined()
            .into_iter()
            .map(|known| {
                let commands = net.start(known.clone(), None, None);
                new_server(known, t!("home.new_server"), commands)
            })
            .collect();
        let runtime = net.handle();
        let dm_deleted_path = data_dir.join("dm_deleted.json");
        let mut dm_deleted: dm::Tombstones =
            crate::load_json(&dm_deleted_path).unwrap_or_else(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "deleted DM messages unreadable");
                dm::Tombstones::default()
            });
        dm_deleted.merge(&net.dm_deleted());
        let synced = device_sync::synced(&audio.settings());
        let layout = layout::Layout::new(&audio.settings());
        // Without servers yet, start by browsing the directory.
        let browse = servers.is_empty();
        let mut friends = Friends::default();
        friends.adopt(&net.contacts());
        Ok(Self {
            identity,
            net,
            view: if browse { View::Find } else { View::Server(0) },
            servers,
            threads: HashMap::new(),
            backfill: None,
            next_backfill: 2.0,
            friends,
            friends_tab: friends::Tab::default(),
            pokes: crate::pokes::Pokes::open(data_dir.join("pokes.json")),
            popup: None,
            requests: Default::default(),
            dm_deleted,
            dm_deleted_path,
            invite: String::new(),
            join: Join::Idle(None),
            find: Find {
                opened: browse,
                ..Find::default()
            },
            dialog: None,
            focus: None,
            toast: None,
            #[cfg(target_os = "android")]
            unsent,
            settings: announce.then_some(Tab::WhatsNew),
            hotkeys: crate::hotkeys::Hotkeys::new(ctx, audio.clone()),
            voice: Voice::new(audio, video, runtime),
            files: Files::new(&data_dir),
            previews: Previews::new(ctx, &data_dir),
            sending: VecDeque::new(),
            emoji_picker: Picker::new(&data_dir),
            switcher: switcher::Switcher::default(),
            confirm_sign_out: false,
            remove_device: None,
            error: None,
            key_storage: Default::default(),
            route_editor: Default::default(),
            route_generation: rusttalk_core::route::generation(),
            server_settings: None,
            kick: None,
            crop: None,
            name_draft: None,
            bio_draft: None,
            welcome: welcome::Step::Idle,
            new_account: false,
            synced_once: false,
            synced,
            left: HashSet::new(),
            status: None,
            idle: false,
            status_unsent: HashSet::new(),
            active_at: 0.0,
            system_idle: crate::idle::SystemIdle::start(),
            layout,
            notify: Notify::open(&data_dir, notify::unix_now()),
            focused: true,
            restored: false,
        })
    }

    /// Do not disturb, picked on this or another of my devices: no notifications or
    /// message sounds here. Unread marks still count, and a call keeps its sounds.
    pub fn dnd(&self) -> bool {
        self.status == Some(Choice::Dnd)
    }

    /// Choose a status on every server, for all my devices.
    fn set_status(&mut self, choice: Choice) {
        self.status = Some(choice);
        self.status_unsent.clear();
        for server in &self.servers {
            if server.status != Status::Online {
                self.status_unsent.insert(server.address.clone());
            }
            server.send(Command::SetStatus(choice));
        }
    }

    /// The status others see: an "Online" choice shows as away while every one of my
    /// devices is idle, as the servers combine them.
    pub fn shown_status(&self) -> Choice {
        let choice = self.status.unwrap_or(Choice::Online);
        let away = (self.servers.iter())
            .filter(|server| server.status == Status::Online)
            .filter_map(|server| server.member(&server.dir.me))
            .any(|member| member.presence == Presence::Away);
        if choice == Choice::Online && away {
            Choice::Away
        } else {
            choice
        }
    }

    /// Tell the servers when this device goes idle after a while without input or call,
    /// and when it is used again.
    fn track_activity(&mut self, ui: &Ui) {
        let (now, input) = ui.input(|input| (input.time, !input.events.is_empty()));
        // In a call people talk while another app has the focus, so the call counts as activity.
        let active = input || self.voice.call.is_some();
        if active {
            self.active_at = now;
        }
        let idle = device_idle(active, now - self.active_at, self.system_idle.seconds());
        if idle != self.idle {
            self.idle = idle;
            for server in &self.servers {
                server.send(Command::SetIdle(idle));
            }
        }
        // Without input egui stops drawing; wake up to notice the time passing.
        ui.ctx().request_repaint_after(Duration::from_secs(30));
    }

    /// Stop showing a server, and leave its call and any dialog that points into the list.
    fn remove_server(&mut self, index: usize) -> Server {
        if self
            .voice
            .call
            .as_ref()
            .is_some_and(|call| call.server == self.servers[index].address)
        {
            call::leave(self);
        }
        self.dialog = None;
        self.server_settings = None;
        let server = self.servers.remove(index);
        self.net.forget(&server.address);
        self.files.forget_full_pictures(&server.address);
        let mut settings = self.voice.audio.settings();
        settings.hide_offline.left(&server.address);
        settings.left(&server.address);
        self.voice.audio.set_settings(settings);
        for thread in self.threads.values_mut() {
            thread.forget(&server.address);
        }
        self.view = match self.view {
            View::Server(i) if i == index => {
                if self.servers.is_empty() {
                    self.find.open();
                    View::Find
                } else {
                    View::Server(0)
                }
            }
            View::Server(i) if i > index => View::Server(i - 1),
            view => view,
        };
        server
    }

    /// Leave a server here and on this person's other devices.
    fn leave_server(&mut self, index: usize) {
        let server = self.remove_server(index);
        self.left.insert(server.address.clone());
        self.net.leave(server.address, server.commands);
    }

    /// The person a DM channel on this server is with.
    fn dm_peer(&self, index: usize, channel: &Id) -> Option<Id> {
        let dms = &self.servers[index].dir.dms;
        dms.iter()
            .find(|dm| dm.channel == *channel)
            .map(|dm| dm.peer)
    }

    /// A channel's chat, or the merged DM it is a copy of.
    fn chat(&mut self, index: usize, channel: Id) -> &mut Chat {
        match self.dm_peer(index, &channel) {
            Some(peer) => &mut self.threads.entry(peer).or_default().chat,
            None => self.servers[index].chats.entry(channel).or_default(),
        }
    }

    /// Each server's DM channel with this person: server index and channel.
    fn replicas(&self, peer: &Id) -> Vec<(usize, Id)> {
        let servers = self.servers.iter().enumerate();
        servers
            .flat_map(|(index, server)| {
                let dms = server.dir.dms.iter().filter(|dm| dm.peer == *peer);
                dms.map(move |dm| (index, dm.channel))
            })
            .collect()
    }

    /// Everyone with a DM, once each however many servers hold a copy.
    fn dm_peers(&self) -> Vec<Id> {
        let mut peers: Vec<Id> = Vec::new();
        let known = (self.servers.iter())
            .flat_map(|server| server.dir.dms.iter().map(|dm| dm.peer))
            .chain(self.threads.keys().copied());
        for peer in known {
            if !peers.contains(&peer) {
                peers.push(peer);
            }
        }
        peers
    }

    /// Servers this person is a member of too, where a DM with them can go.
    /// Stored files a message to `at` may carry: a DM goes to every shared server, so
    /// the fewest any of them takes.
    pub(crate) fn max_files(&self, at: At) -> usize {
        match at {
            At::Channel(index, _) => self.servers[index].max_files,
            At::Dm(peer) => (self.shared(&peer).into_iter())
                .map(|index| self.servers[index].max_files)
                .min()
                .unwrap_or(rusttalk_core::messages::LEGACY_MAX_ATTACHMENTS),
        }
    }

    fn shared(&self, peer: &Id) -> Vec<usize> {
        (0..self.servers.len())
            .filter(|index| self.servers[*index].member(peer).is_some())
            .collect()
    }

    /// The names of the servers this person shares with them, for under their name.
    fn shared_names(&self, peer: &Id) -> String {
        // A server saved twice, or two with one name, reads as one.
        let mut shared: Vec<&str> = Vec::new();
        for index in self.shared(peer) {
            let server = self.servers[index].name();
            if !shared.contains(&server) {
                shared.push(server);
            }
        }
        if shared.is_empty() {
            t!("home.no_shared_server").to_owned()
        } else {
            shared.join(", ")
        }
    }

    /// The server whose names and pictures a DM shows: a shared one holding a copy,
    /// else any shared one, else one still holding a copy; online ones first.
    fn dm_server(&self, peer: &Id) -> Option<usize> {
        let shared = self.shared(peer);
        let copies: Vec<usize> = self.replicas(peer).iter().map(|(i, _)| *i).collect();
        let picks: Vec<usize> = (shared.iter().filter(|i| copies.contains(i)))
            .chain(&shared)
            .chain(&copies)
            .copied()
            .collect();
        let online = |index: &&usize| self.servers[**index].status == Status::Online;
        picks.iter().find(online).or(picks.first()).copied()
    }

    fn peer_name(&self, peer: &Id) -> String {
        match self.dm_server(peer) {
            Some(index) => self.servers[index].member_name(peer),
            // Servers remember the names of people who left.
            None => (self
                .servers
                .iter()
                .find_map(|server| server.names.get(peer)))
            .cloned()
            .unwrap_or_else(|| t!("server.member_unnamed", id = &fingerprint(peer)[..4])),
        }
    }

    /// Load every DM on a server that is online, so friend requests sent while this
    /// device was away show up without opening each chat.
    // shortcut: only each DM's newest page is read; a request buried under a long chat waits
    // until that chat is scrolled back.
    fn load_dms(&mut self, index: usize) {
        let server = &self.servers[index];
        if server.status != Status::Online {
            return;
        }
        for dm in &server.dir.dms {
            let thread = self.threads.entry(dm.peer).or_default();
            if thread.open(&server.address, dm.channel) {
                server.send(Command::Open(dm.channel));
            }
        }
    }

    fn restore_pages(&mut self, index: usize, pages: Vec<(Id, Vec<Msg>)>) {
        let server = &mut self.servers[index];
        for (channel, items) in pages {
            if let Some(dm) = server.dir.dms.iter().find(|dm| dm.channel == channel) {
                self.threads.entry(dm.peer).or_default().add(
                    &server.address,
                    channel,
                    items,
                    false,
                    None,
                    &dm::Pair {
                        me: *self.identity.certificate.person_id(),
                        peer: dm.peer,
                        deleted: &self.dm_deleted,
                    },
                );
            } else if server
                .dir
                .categories
                .iter()
                .flat_map(|c| &c.channels)
                .any(|c| c.id == channel)
            {
                server.chats.entry(channel).or_default().merge(items, false);
            }
        }
    }

    /// Delete my DM message on every server holding it that is online. The delete
    /// is kept and synced to my other devices, so a server that is away deletes it
    /// once it shows it again, also after a restart (see `Thread::add`).
    fn delete_dm(&mut self, peer: Id, id: Id, now: f64) {
        let me = *self.identity.certificate.person_id();
        // Only a readable copy shows I wrote it; a placeholder could be anyone's forgery.
        let Some(msg) = (self.threads.get(&peer)).and_then(|thread| {
            (thread.chat.messages.iter())
                .find(|m| m.id == id && m.person == me && m.body != Body::Unreadable)
        }) else {
            return;
        };
        let msg = msg.clone();
        // Each copy has its own stored files: the shown copy's, then the others'.
        let files: Vec<Id> = (msg.files.iter())
            .flat_map(|file| std::iter::once(&file.file).chain(&file.thumbnail))
            .map(|blob| blob.id)
            .collect();
        let mut sent = false;
        for (index, channel) in self.replicas(&peer) {
            let server = &self.servers[index];
            if server.status != Status::Online || !msg.replicas.contains(&server.address) {
                continue;
            }
            let files = if msg.replicas[0] == server.address {
                files.clone()
            } else {
                // Each copy has its own blobs.
                (msg.copy_files.iter())
                    .filter(|(held, _)| *held == server.address)
                    .flat_map(|(_, attachments)| attachments)
                    .flat_map(|file| std::iter::once(&file.file).chain(&file.thumbnail))
                    .map(|blob| blob.id)
                    .collect()
            };
            server.send(Command::Delete(channel, id, me, files));
            sent = true;
        }
        if !sent {
            self.toast = Some((t!("home.waiting_try_again").into(), now + 6.0));
        }
        // Each copy by server and DM channel there.
        let copies: Vec<String> = (self.replicas(&peer).into_iter())
            .map(|(index, channel)| (&self.servers[index].address, channel))
            .filter(|(server, _)| msg.replicas.contains(server))
            .map(|(server, channel)| dm::copy(server, &channel))
            .collect();
        if self.dm_deleted.add(id, copies.clone()) {
            self.record_delete(Edit::DeletedDm(id, copies));
        }
        if let Some(thread) = self.threads.get_mut(&peer) {
            thread.deleted(&me, &id);
            let chat = &mut thread.chat;
            if chat.reply_to == Some((me, id)) {
                chat.reply_to = None;
            }
            if chat.editing == Some(id) {
                chat.editing = None;
                chat.clear_draft();
            }
        }
    }

    /// Keep my deleted DM messages here, and send a change to my other devices.
    fn record_delete(&mut self, edit: Edit) {
        let data = self.dm_deleted_path.parent().expect("in the data folder");
        let saved = rusttalk_core::client::save_json(data, &self.dm_deleted_path, &self.dm_deleted);
        if let Err(error) = saved {
            tracing::warn!(error = %format_args!("{error:#}"), "saving deleted DM messages failed");
        }
        self.net.sync(vec![edit]);
    }

    pub fn remove_device(
        &mut self,
        record: &rusttalk_core::identity::Revocation,
    ) -> anyhow::Result<()> {
        self.net.remove_device(record)?;
        for dir in self.servers.iter_mut().flat_map(Server::dirs_mut) {
            for member in dir.members.edit().iter_mut() {
                if &member.person == record.person_id() {
                    member
                        .devices
                        .retain(|d| &d.signing_key != record.device_signing_key());
                }
            }
        }
        Ok(())
    }

    /// Publish the synced settings that changed since the last sync.
    pub fn publish_settings(&mut self) {
        let mut current = device_sync::synced(&self.voice.audio.settings());
        current.extend(self.notify.synced());
        let edits: Vec<_> = current
            .iter()
            .filter(|(key, value)| self.synced.get(*key) != Some(value))
            .map(|(key, value)| Edit::Setting(key.clone(), value.clone()))
            .collect();
        if !edits.is_empty() {
            self.net.sync(edits);
            self.synced = current;
        }
    }

    /// Each server's name for me, or `None` while it hasn't loaded, for `migrate`.
    fn migrate_names(&self, settings: &mut crate::audio::Settings) {
        let names: Vec<_> = (self.servers.iter())
            .map(|server| {
                let loaded = server.profile_ready();
                let name = server
                    .member(&server.dir.me)
                    .and_then(|m| m.name.as_deref());
                (&server.address, loaded.then_some(name))
            })
            .collect();
        settings.migrate(&names);
    }

    /// Change my name or bio settings, migrating first if that never happened, and
    /// sync them. `change` returns what to tell which servers now; offline servers
    /// hear it from what stays pending once they load.
    pub fn change_profile(
        &mut self,
        change: impl FnOnce(&mut crate::audio::Settings, Vec<url::Url>) -> Vec<(url::Url, Command)>,
    ) {
        let mut settings = self.voice.audio.settings();
        if settings.display_name.is_empty() {
            self.migrate_names(&mut settings);
        }
        let servers = self.servers.iter().map(|s| s.address.clone()).collect();
        for (address, command) in change(&mut settings, servers) {
            if let Some(index) = self.index(&address)
                && self.servers[index].status == Status::Online
            {
                self.servers[index].send(command);
            }
        }
        self.voice.audio.set_settings(settings);
        self.publish_settings();
    }

    /// A server loaded my name and bio there: migrate once every server has loaded,
    /// and send what it still has to hear.
    fn profile_loaded(&mut self, index: usize) {
        let mut settings = self.voice.audio.settings();
        if settings.display_name.is_empty() && self.servers.iter().all(Server::profile_ready) {
            self.migrate_names(&mut settings);
        }
        let server = &self.servers[index];
        if let Some(mine) = server
            .member(&server.dir.me)
            .filter(|_| server.profile_ready())
        {
            let (name, bio) = settings.reported(
                &server.address,
                mine.name.as_deref(),
                mine.bio.as_deref(),
                server.dir.bios,
            );
            if let Some(name) = name {
                server.send(Command::SetDisplayName(name));
            }
            if let Some(bio) = bio {
                server.send(Command::SetBio(bio));
            }
        }
        self.voice.audio.set_settings(settings);
        self.publish_settings();
    }

    /// The chat on screen: server index and channel or DM.
    fn viewing(&self) -> Vec<(usize, Id)> {
        if self.covered() {
            return Vec::new();
        }
        match self.view {
            View::Server(index) => (self.servers[index].open)
                .map(|channel| (index, channel))
                .into_iter()
                .collect(),
            // A DM shows its copy on every server.
            View::Direct(Some(peer)) => self.replicas(&peer),
            View::Direct(None) | View::Pokes | View::Join | View::Find => Vec::new(),
        }
    }

    /// The server whose member list can show beside what is open.
    fn member_server(&self) -> Option<usize> {
        match self.view {
            View::Server(index)
                if self.servers[index].status != Status::Connecting
                    && self.servers[index].compatibility != Compatibility::AppTooOld =>
            {
                Some(index)
            }
            _ => None,
        }
    }

    /// Back after menus and dialogs (`crate::back`): the member list over the chat
    /// closes, then a phone goes from what is open to the list, like its Back bar.
    /// False at the list.
    pub fn go_back(&mut self) -> bool {
        if self.voice.stage.full.is_some() {
            self.voice.stage.full = None;
        } else if self.members_over_chat() {
            self.layout.toggle_members();
        } else if self.layout.phone && !self.layout.list {
            self.layout.list = true;
        } else {
            return false;
        }
        true
    }

    /// Whether `go_back` has anywhere to go.
    pub fn can_go_back(&self) -> bool {
        self.voice.stage.full.is_some()
            || self.members_over_chat()
            || (self.layout.phone && !self.layout.list)
    }

    fn members_over_chat(&self) -> bool {
        !(self.layout.phone && self.layout.list)
            && self.member_server().is_some()
            && self.layout.peeking()
    }

    /// Phones: what is open is behind the list or the member list, so nobody reads
    /// it. From the navigation as it is now, so a message right after Back counts.
    fn covered(&self) -> bool {
        self.layout.phone
            && (self.layout.list || (self.member_server().is_some() && self.layout.members_shown()))
    }

    /// Show a chat, like after a click on its notification.
    pub(crate) fn open_chat(&mut self, address: &url::Url, channel: Id) {
        let Some(index) = self.index(address) else {
            return;
        };
        if let Some(peer) = self.dm_peer(index, &channel) {
            self.view = View::Direct(Some(peer));
            self.layout.list = false;
            return;
        }
        let server = &mut self.servers[index];
        if server.channel(&channel).is_some() {
            server.open = Some(channel);
            self.view = View::Server(index);
            self.layout.list = false;
        } else if (server.dir.calendar.iter())
            .any(|c| c.events.iter().any(|e| e.record.id == channel))
        {
            // A reminder: its ID is the event's.
            server.open = None;
            server.events_open = true;
            self.view = View::Server(index);
            self.layout.list = false;
        }
    }

    /// Notify once about each new reminder to answer an event, unless the server is muted.
    fn remind(&mut self, ctx: &egui::Context, index: usize) {
        let server = &self.servers[index];
        let now = notify::unix_now();
        self.notify.expire_reminders(now);
        let prefs = self.notify.prefs();
        let rule = prefs.server(&server.address);
        if !prefs.desktop
            || self.dnd()
            || rule.muted(now)
            || rule.level == Some(notify::Level::Nothing)
        {
            return;
        }
        let due = events::reminders(server, now);
        let address = server.address.clone();
        for (id, at, remove_at, body) in due {
            if self
                .notify
                .new_reminder(&address, &id, at, remove_at, self.focused)
            {
                let title = t!("events.reminder_title", server = self.servers[index].name());
                self.notify.show(ctx, &title, &body, (address.clone(), id));
            }
        }
    }

    /// A new message: mark it unread, count a mention and notify, as the person chose.
    fn arrived(&mut self, ctx: &egui::Context, index: usize, channel: Id, msg: Msg) {
        if msg.backfilled {
            return;
        }
        // Voice requests ring instead, and this person's own answers count too.
        if matches!(msg.body, Body::VoiceRequest(_)) {
            if let Some(peer) = self.dm_peer(index, &channel) {
                self.request_arrived(ctx, (index, channel), peer, &msg);
            }
            return;
        }
        let me = *self.identity.certificate.person_id();
        if msg.person == me
            || msg.edit_of.is_some()
            || matches!(msg.body, Body::Notice { .. } | Body::PollPart(_))
        {
            return;
        }
        let peer = self.dm_peer(index, &channel);
        let dm = peer.is_some();
        // Pokes are listed apart from the chat; they too only mean something in a DM.
        if matches!(msg.body, Body::Poke(_)) {
            if let Some(peer) = peer {
                self.poke_arrived(ctx, (index, channel), peer, &msg, true);
            }
            return;
        }
        // Friend messages only mean something in a DM. Blocked people's messages in
        // shared channels don't mark, mention or notify either.
        if !dm && (matches!(msg.body, Body::Friend(..)) || self.is_blocked(&msg.person)) {
            return;
        }
        if let Some(peer) = peer {
            self.friend_message(peer, &msg);
            // Nothing from blocked people; friend answers only change the list. With
            // "friends only", other people's messages wait quietly under requests.
            let relation = self.friends.relation(&peer);
            let quiet = match msg.body {
                _ if relation == Relation::Blocked => true,
                Body::Friend(signal, _) => {
                    signal != FriendSignal::Request || relation != Relation::Incoming
                }
                _ => self.friends.is_request(&peer, self.friends_only()),
            };
            // A DM arrives once per server holding a copy: count and notify it once, and
            // only for a copy that opened and comes from the other person.
            if quiet
                || msg.person != peer
                || !(self.threads.entry(peer).or_default()).first_arrival(&msg)
            {
                return;
            }
        }
        let server = &self.servers[index];
        let my_roles = server.member(&me).map_or(&[][..], |member| &member.roles);
        let message = mentions::Message::new(&msg, &server.dir, channel);
        let arrival = Arrival {
            dm,
            mention: notify::mention(&message, &me, my_roles),
        };
        let prefs = self.notify.prefs();
        let rule = prefs.effective(&server.address, &channel, dm, notify::unix_now());
        let viewing = self.viewing().contains(&(index, channel));
        let mention = !(viewing && self.focused) && notify::counts(rule, arrival);
        let alert = !self.dnd();
        // Our own chime is heard even when the system keeps its notifications quiet.
        let chimed = alert && mention && prefs.sound && self.voice.audio.message();
        if alert && notify::should_notify(prefs, rule, arrival, self.focused, viewing) {
            let sender = server.member_name(&msg.person);
            let title = match server.channel(&channel) {
                Some(found) if !dm => t!(
                    "notify.title_in_channel",
                    name = sender,
                    channel = found.name,
                    server = server.name()
                ),
                _ => sender,
            };
            // Mentions read as names, like in the chat.
            let body = match &msg.body {
                Body::Friend(..) => t!("notify.friend_request").into(),
                Body::Text(_) if msg.sticker.is_some() => t!("notify.sent_sticker").into(),
                Body::Poll(question, _) => format!("📊 {question}"),
                Body::Text(text) if !text.trim().is_empty() => message.plain(&server.dir),
                _ if !msg.files.is_empty() || msg.live.is_some() => t!("notify.sent_file").into(),
                _ => t!("notify.new_message").into(),
            };
            let target = (server.address.clone(), channel);
            if chimed {
                self.notify.show_silent(ctx, &title, &body, target);
            } else {
                self.notify.show(ctx, &title, &body, target);
            }
        }
        let address = self.servers[index].address.clone();
        self.notify.arrived(&address, channel, msg.at, mention);
    }

    /// Change a server's notification rule, or a channel's.
    fn set_rule(&mut self, index: usize, channel: Option<Id>, rule: notify::Rule) {
        let mut prefs = self.notify.prefs().clone();
        prefs.set(&self.servers[index].address, channel.as_ref(), rule);
        self.notify.set_prefs(prefs);
        self.publish_settings();
    }

    /// Unread channels and mentions in a server's channel list; muted channels don't count.
    fn server_marks(&self, index: usize) -> (bool, u32) {
        let server = &self.servers[index];
        let (prefs, now) = (self.notify.prefs(), notify::unix_now());
        let channels = (server.dir.categories.iter())
            .flat_map(|category| &category.channels)
            .filter(|channel| {
                !prefs
                    .effective(&server.address, &channel.id, false, now)
                    .muted
            });
        channels.fold((false, 0), |(unread, mentions), channel| {
            (
                unread || self.notify.unread(&server.address, &channel.id),
                mentions + self.notify.mentions(&server.address, &channel.id),
            )
        })
    }

    /// The same for the DMs on every server; blocked people and message requests don't count.
    fn dm_marks(&self) -> (bool, u32) {
        let (prefs, now) = (self.notify.prefs(), notify::unix_now());
        let friends_only = self.friends_only();
        let dms = (self.servers.iter()).flat_map(|server| {
            (server.dir.dms.iter().map(move |dm| (&server.address, dm))).filter(move |(_, dm)| {
                !prefs
                    .effective(&server.address, &dm.channel, true, now)
                    .muted
                    && self.friends.relation(&dm.peer) != Relation::Blocked
                    && !self.friends.is_request(&dm.peer, friends_only)
            })
        });
        dms.fold((false, 0), |(unread, mentions), (server, dm)| {
            (
                unread || self.notify.unread(server, &dm.channel),
                mentions + self.notify.mentions(server, &dm.channel),
            )
        })
    }

    pub(crate) fn edit_channel(&mut self, index: usize, id: Id, edit: ChannelEdit) {
        let server = &self.servers[index];
        if !server.dir.perms.manage_channels || server.status != Status::Online {
            return;
        }
        if let Some(channel) = server.channel(&id) {
            self.dialog = Some(match edit {
                ChannelEdit::Rename => Dialog::EditChannel {
                    server: index,
                    id,
                    tab: ChannelTab::General,
                    name: channel.name.clone(),
                    topic: channel.topic.clone(),
                    topic_changed: false,
                    audio: channel.voice.then_some((channel.audio_kbps, false)),
                    limit: channel.voice.then_some((channel.member_limit, false)),
                    flags: Vec::new(),
                },
                ChannelEdit::Delete => {
                    Dialog::DeleteItem(index, reorder::Item::Channel(id), channel.name.clone())
                }
            });
        }
    }

    /// Send queued messages whose preview is done, failed or removed, in order per chat.
    fn send_queued(&mut self, ctx: &egui::Context, now: f64) {
        // Turned off meanwhile: stop what is being made, and don't ask.
        if self.voice.audio.snapshot().create_link_previews == Some(false) {
            for out in &mut self.sending {
                out.preview = None;
            }
        }
        let mut waiting: Vec<To> = Vec::new();
        let mut at = 0;
        while at < self.sending.len() {
            let out = &self.sending[at];
            let done = match &out.preview {
                _ if waiting.contains(&out.to) => None,
                None => Some(None),
                Some(Making::Ask(_)) => None,
                Some(Making::Job(job)) => job.poll(),
            };
            let Some(made) = done else {
                waiting.push(out.to.clone());
                ctx.request_repaint_after(std::time::Duration::from_millis(250));
                at += 1;
                continue;
            };
            let Some(Outgoing { to, mut draft, .. }) = self.sending.remove(at) else {
                break;
            };
            draft.preview = made.map(Arc::new);
            match to {
                To::Channel(address, channel) => {
                    if let Some(index) = self.index(&address) {
                        let server = &mut self.servers[index];
                        let chat = server.chats.entry(channel).or_default();
                        forum::dispatch(chat, &server.commands, draft);
                    }
                }
                To::Dm(peer) => {
                    let targets: Vec<usize> = (self.shared(&peer).into_iter())
                        .filter(|index| self.servers[*index].status == Status::Online)
                        .collect();
                    let me = *self.identity.certificate.person_id();
                    let chat = &mut self.threads.entry(peer).or_default().chat;
                    send_dm(
                        &self.servers,
                        &targets,
                        chat,
                        &mut self.toast,
                        now,
                        (me, peer),
                        draft,
                    );
                }
            }
        }
    }

    fn index(&self, address: &url::Url) -> Option<usize> {
        self.servers
            .iter()
            .position(|server| &server.address == address)
    }

    fn apply(&mut self, ctx: &egui::Context, update: Update, now: f64) {
        // While a server is previewed, what replaces its directory lands on the real one,
        // and the preview is made again from it.
        let lifted = match &update {
            Update::Directory(address, _)
            | Update::Cached(address, ..)
            | Update::Inaccessible(address, _)
            | Update::Status(address, _) => self.index(address).and_then(|index| {
                let roles = view_as::exit(&mut self.servers[index])?;
                Some((address.clone(), roles))
            }),
            _ => None,
        };
        self.apply_update(ctx, update, now);
        if let Some((address, roles)) = lifted
            && let Some(index) = self.index(&address)
            && !matches!(self.servers[index].status, Status::Stopped(_))
        {
            view_as::show_as(&mut self.servers[index], roles);
        }
    }

    fn apply_update(&mut self, ctx: &egui::Context, update: Update, now: f64) {
        match update {
            Update::Preview(result) => {
                self.join = match result {
                    Ok(preview) => Join::Preview {
                        preview,
                        name: self.voice.audio.settings().display_name,
                    },
                    Err(error) => Join::Idle(Some(error)),
                }
            }
            Update::Found(page, found) => {
                let find = &mut self.find;
                find.searching = false;
                match found {
                    Ok(results) => {
                        find.more = results.len() == PAGE_SIZE;
                        find.page = page;
                        if page == 1 {
                            find.results = results;
                        } else {
                            find.results.extend(results);
                        }
                    }
                    Err(error) => find.error = Some(error),
                }
            }
            Update::Listed(checked) => {
                self.find.checking = false;
                match checked {
                    Ok(preview) => {
                        self.join = Join::Preview {
                            preview,
                            name: self.voice.audio.settings().display_name,
                        };
                        self.view = View::Join;
                    }
                    // An official server, picked on the join page.
                    Err(error) if matches!(self.join, Join::Checking) => {
                        self.join = Join::Idle(Some(error));
                    }
                    Err(error) => self.find.error = Some(error),
                }
            }
            Update::Cached(address, dir, pages) => {
                if let Some(i) = self.index(&address) {
                    let server = &mut self.servers[i];
                    server.dir = *dir;
                    for (channel, items) in &pages {
                        if !items.is_empty()
                            && !server.is_voice(channel)
                            && !server.dir.dms.iter().any(|dm| dm.channel == *channel)
                        {
                            server.open = Some(*channel);
                        }
                    }
                    self.restore_pages(i, pages);
                }
            }
            Update::Inaccessible(address, ids) => {
                if let Some(i) = self.index(&address) {
                    self.files.forget_full_pictures(&address);
                    let server = &mut self.servers[i];
                    for dm in &server.dir.dms {
                        if ids.contains(&dm.channel)
                            && let Some(thread) = self.threads.get_mut(&dm.peer)
                        {
                            thread.forget(&address);
                        }
                    }
                    server.chats.retain(|id, _| !ids.contains(id));
                    server.dir.discard_channels(&ids);
                    if server.open.is_some_and(|id| ids.contains(&id)) {
                        server.open = None;
                    }
                }
            }
            Update::ResetHistory(address, pages) => {
                if let Some(i) = self.index(&address) {
                    self.files.forget_full_pictures(&address);
                    for (channel, _) in &pages {
                        if let Some(peer) = self.dm_peer(i, channel) {
                            if let Some(thread) = self.threads.get_mut(&peer) {
                                thread.forget(&address);
                            }
                        } else if let Some(chat) = self.servers[i].chats.get_mut(channel) {
                            chat.clear_history();
                        }
                    }
                    self.restore_pages(i, pages);
                    self.load_dms(i);
                }
            }
            Update::Status(address, status) => {
                let Some(index) = self.index(&address) else {
                    return;
                };
                if status != Status::Online
                    && self
                        .kick
                        .as_ref()
                        .is_some_and(|kick| kick.belongs_to(&self.servers[index]))
                {
                    self.kick = None;
                }
                let joining = matches!(&self.join, Join::Joining(url) if *url == address);
                match (&status, joining) {
                    (Status::Online, true) => {
                        self.join = Join::Idle(None);
                        self.invite.clear();
                        self.view = View::Server(index);
                    }
                    (Status::Stopped(error), true) => {
                        self.join = if self.servers[index].compatibility == Compatibility::AppTooOld
                        {
                            Join::UpdateRequired
                        } else {
                            Join::Idle(Some(error.clone()))
                        };
                        self.remove_server(index);
                        self.view = View::Join;
                        return;
                    }
                    _ => {}
                }
                if matches!(status, Status::Stopped(_))
                    && self
                        .voice
                        .call
                        .as_ref()
                        .is_some_and(|c| c.server == address)
                {
                    call::leave(self);
                }
                let server = &mut self.servers[index];
                if status == Status::Online {
                    self.next_backfill = now;
                    server.order_unavailable = false;
                    // Opens that failed while offline.
                    for (channel, chat) in &server.chats {
                        if chat.loading
                            && chat
                                .messages
                                .iter()
                                .all(|m| matches!(m.body, Body::Notice { .. }))
                        {
                            let _ = server.commands.send(Command::Open(*channel));
                        }
                    }
                    for thread in self.threads.values() {
                        for channel in thread.unanswered(&address) {
                            server.send(Command::Open(channel));
                        }
                    }
                }
                if matches!(status, Status::Stopped(_)) {
                    self.files.forget_full_pictures(&address);
                    server.chats.clear();
                    server.names.clear();
                    server.dir = Directory::default();
                    for thread in self.threads.values_mut() {
                        thread.forget(&address);
                    }
                }
                server.status = status;
                self.load_dms(index);
            }
            Update::Compatibility(address, compatibility) => {
                if let Some(index) = self.index(&address) {
                    self.servers[index].compatibility = compatibility;
                }
            }
            Update::MaxFiles(address, max) => {
                if let Some(index) = self.index(&address) {
                    self.servers[index].max_files = max;
                }
            }
            Update::ChannelOrderUnavailable(address) => {
                if let Some(index) = self.index(&address) {
                    self.servers[index].order_unavailable = true;
                }
            }
            Update::Directory(address, dir) => {
                if let Some(index) = self.index(&address) {
                    let peers: HashSet<_> = self
                        .servers
                        .iter()
                        .flat_map(|s| s.dir.dms.iter().map(|dm| dm.peer))
                        .collect();
                    let server = &mut self.servers[index];
                    for member in &dir.members {
                        if let Some(name) = &member.name {
                            server.names.insert(member.person, name.clone());
                        }
                    }
                    // The latest pick wins: one made here while this server was offline is
                    // sent now; otherwise a different stored one was picked on another device.
                    let unsent = self.status_unsent.remove(&address);
                    match (self.status, dir.status) {
                        (Some(mine), Some(theirs)) if mine != theirs && unsent => {
                            server.send(Command::SetStatus(mine));
                        }
                        (_, Some(theirs)) => self.status = Some(theirs),
                        _ => {}
                    }
                    // A reconnect starts this device active on the server.
                    if self.idle {
                        server.send(Command::SetIdle(true));
                    }
                    if let Some(theirs) = dir.hide_offline {
                        let mut settings = self.voice.audio.settings();
                        if let Some(on) = settings.hide_offline.reported(&address, theirs) {
                            server.send(Command::SetHideOffline(on));
                        }
                        self.voice.audio.set_settings(settings);
                    }
                    let me = dir.me;
                    let muted = (dir.members.iter()).any(|m| m.person == me && m.voice_muted);
                    let changed = server.voice_muted(&me) != muted;
                    let dm_members = |dir: &Directory| {
                        dir.members
                            .iter()
                            .map(|m| m.person)
                            .filter(|id| peers.contains(id))
                            .collect::<HashSet<_>>()
                    };
                    if dm_members(&server.dir) != dm_members(&dir)
                        || server
                            .dir
                            .dms
                            .iter()
                            .map(|dm| (dm.peer, dm.channel))
                            .collect::<HashSet<_>>()
                            != dir.dms.iter().map(|dm| (dm.peer, dm.channel)).collect()
                    {
                        self.next_backfill = now;
                    }
                    if let Some(old) = server.dir.notice_channel()
                        && Some(old) != dir.notice_channel()
                        && let Some(chat) = server.chats.get_mut(&old)
                    {
                        chat.messages
                            .retain(|msg| !matches!(msg.body, Body::Notice { .. }));
                    }
                    for old in &server.dir.dms {
                        if !dir
                            .dms
                            .iter()
                            .any(|dm| dm.peer == old.peer && dm.channel == old.channel)
                            && let Some(thread) = self.threads.get_mut(&old.peer)
                        {
                            thread.forget(&address);
                        }
                    }
                    server.chats.retain(|id, _| {
                        dir.categories
                            .iter()
                            .flat_map(|c| &c.channels)
                            .any(|c| &c.id == id)
                    });
                    server.dir = *dir;
                    self.remind(ctx, index);
                    self.profile_loaded(index);
                    if changed
                        && muted
                        && (self.voice.call.as_ref()).is_some_and(|call| call.server == address)
                    {
                        self.toast = Some((t!("call.server_muted").into(), now + 6.0));
                    }
                    self.load_dms(index);
                }
            }
            Update::Calendar(address, result) => {
                if let Some(index) = self.index(&address) {
                    self.servers[index].events.changed(result);
                }
            }
            Update::Left(address) => {
                if let Some(index) = self.index(&address) {
                    self.leave_server(index);
                }
            }
            Update::Chosen(address, choice) => {
                self.status = Some(choice);
                self.status_unsent.remove(&address);
            }
            Update::Presence(address, person, presence) => {
                if let Some(index) = self.index(&address) {
                    for dir in self.servers[index].dirs_mut() {
                        let mut members = dir.members.edit();
                        if let Some(member) = members.iter_mut().find(|m| m.person == person) {
                            member.presence = presence;
                        }
                    }
                }
            }
            Update::SaveProgress(id, done) => self.files.progress(id, done),
            Update::Messages {
                server,
                channel,
                items,
                older,
                page,
            } => {
                let at_start = page.map(|(start, _)| start);
                let Some(index) = self.index(&server) else {
                    return;
                };
                if let Some(peer) = self.dm_peer(index, &channel) {
                    for msg in &items {
                        if matches!(
                            msg.body,
                            Body::VoiceRequest(rusttalk_core::messages::VoiceRequest::Answer(..))
                        ) {
                            self.request_arrived(ctx, (index, channel), peer, msg);
                        }
                        self.friend_message(peer, msg);
                        // The newest page holds pokes sent while this device was away.
                        if !older {
                            self.poke_arrived(ctx, (index, channel), peer, msg, false);
                        }
                    }
                    let me = *self.identity.certificate.person_id();
                    let thread = self.threads.entry(peer).or_default();
                    if !older && at_start.is_some() {
                        thread.forget(&server);
                        thread.open(&server, channel);
                    }
                    if older && !items.is_empty() {
                        thread.chat.keep_scroll = Some(thread.chat.content_height);
                    }
                    let pair = dm::Pair {
                        me,
                        peer,
                        deleted: &self.dm_deleted,
                    };
                    // I deleted it while this server was away: delete it here too.
                    for (id, files) in thread.add(&server, channel, items, older, at_start, &pair) {
                        let stale = Command::Delete(channel, id, me, files);
                        self.servers[index].send(stale);
                        // Kept until that server confirms, also across restarts.
                        let copy = dm::copy(&server, &channel);
                        if self.dm_deleted.add(id, [copy.clone()]) {
                            let edit = Edit::DeletedDm(id, vec![copy]);
                            self.record_delete(edit);
                        }
                    }
                    return;
                }
                let chat = self.servers[index].chats.entry(channel).or_default();
                if older && !items.is_empty() {
                    chat.keep_scroll = Some(chat.content_height);
                }
                if !older && at_start.is_some() {
                    let notices: Vec<_> = chat
                        .messages
                        .extract_if(.., |m| matches!(m.body, Body::Notice { .. }))
                        .collect();
                    chat.clear_history();
                    chat.messages = notices;
                }
                chat.merge(items, older);
                if let Some((at_start, oldest)) = page {
                    chat.oldest = oldest;
                    chat.loading = false;
                    chat.at_start |= at_start;
                }
            }
            Update::Arrived(address, channel, msg) => {
                if let Some(index) = self.index(&address) {
                    self.arrived(ctx, index, channel, msg);
                }
            }
            Update::Latest(address, found) => {
                for (channel, at) in found {
                    self.notify.arrived(&address, channel, at, false);
                }
            }
            Update::Deleted(address, channel, id) => {
                let Some(index) = self.index(&address) else {
                    return;
                };
                let me = *self.identity.certificate.person_id();
                let dm = self.dm_peer(index, &channel);
                // One of my deletes may be done there: check before counting it.
                if dm.is_some() && self.dm_deleted.contains(&id) {
                    self.servers[index].send(Command::CheckGone(channel, id));
                }
                let chat = match dm {
                    Some(peer) => self.threads.get_mut(&peer).map(|thread| {
                        thread.remove(&address, &id);
                        &mut thread.chat
                    }),
                    None => self.servers[index].chats.get_mut(&channel),
                };
                if let Some(chat) = chat {
                    if dm.is_none() {
                        chat.remove(&id);
                    }
                    // In a DM another server's copy may still show it.
                    let shown = |person: Id| {
                        (chat.messages.iter()).any(|msg| msg.id == id && msg.person == person)
                    };
                    if let Some((person, replied)) = chat.reply_to
                        && replied == id
                        && !shown(person)
                    {
                        chat.reply_to = None;
                    }
                    if chat.editing == Some(id) && !shown(me) {
                        chat.editing = None;
                        chat.clear_draft();
                    }
                }
            }
            Update::CopyGone(address, channel, id) => {
                let copy = dm::copy(&address, &channel);
                if self.dm_deleted.gone(&id, &copy) {
                    self.record_delete(Edit::DmCopyGone(id, copy));
                }
            }
            Update::RetryDmBackfill => {
                self.next_backfill = self.next_backfill.min(now + 30.0);
                ctx.request_repaint_after(Duration::from_secs(30));
            }
            Update::PageFailed(address, channel, missing) => {
                if let Some(index) = self.index(&address)
                    && let Some(peer) = self.dm_peer(index, &channel)
                    && let Some(thread) = self.threads.get_mut(&peer)
                {
                    thread.page_failed(&address, channel, missing);
                }
            }
            Update::Notice(address, channel, msg) => {
                if let Some(index) = self.index(&address) {
                    self.servers[index]
                        .chats
                        .entry(channel)
                        .or_default()
                        .merge(vec![msg], false);
                }
            }
            Update::DmReady(address, channel) => {
                if let Some(index) = self.index(&address)
                    && let Some(peer) = self.dm_peer(index, &channel)
                {
                    self.view = View::Direct(Some(peer));
                }
            }
            Update::Invite(address, id, link) => {
                if let Some(Dialog::Invite(invite)) = &mut self.dialog
                    && self.servers[invite.server].address == address
                {
                    invite.making = false;
                    invite.link = Some((id, link));
                }
            }
            Update::Admin(address, update) => {
                let mut block = None;
                match &mut self.server_settings {
                    Some(settings) if self.servers[settings.server].address == address => {
                        let done = matches!(update, AdminUpdate::Done(_));
                        settings.apply(update);
                        block = done.then(|| settings.banned_to_block()).flatten();
                    }
                    _ => {
                        if let AdminUpdate::Failed(text) | AdminUpdate::PartlySaved { text, .. } =
                            update
                        {
                            self.toast = Some((text, now + 6.0));
                        }
                    }
                }
                if let Some(person) = block {
                    self.block(person, now);
                }
            }
            Update::Call(address, channel, state) => {
                let Some(call) = &mut self.voice.call else {
                    return;
                };
                if call.server != address || call.channel != channel {
                    return;
                }
                match state {
                    Some(state) => {
                        let was_reconnecting = call.state == crate::server::CallState::Reconnecting;
                        call.state = state;
                        if state == crate::server::CallState::Reconnecting {
                            self.voice.sounds.reconnecting();
                            if let Some(watching) = &mut self.voice.watching {
                                watching.grace_until = None;
                            }
                        } else if was_reconnecting
                            && state == crate::server::CallState::Connected
                            && let Some(watching) = &mut self.voice.watching
                        {
                            // Remote sharers may finish rejoining and republishing shortly after us.
                            watching.grace_until = Some(
                                std::time::Instant::now() + std::time::Duration::from_secs(15),
                            );
                        }
                    }
                    None => self.voice.end_call(),
                }
            }
            Update::VideoReplaced(address, channel, old, new) => {
                if self
                    .voice
                    .call
                    .as_ref()
                    .is_some_and(|call| call.server == address && call.channel == channel)
                {
                    self.voice.replace_video(&old, &new);
                }
            }
            Update::CanTalk(address, channel, can_talk) => {
                if let Some(call) = &mut self.voice.call
                    && call.server == address
                    && call.channel == channel
                {
                    call.can_talk = can_talk;
                }
            }
            Update::VoiceMoved(address, from, to) => {
                let Some(index) = self.index(&address) else {
                    return;
                };
                // The removal from the old room may have ended the call already; the network
                // task only passes moves for this device's call.
                if (self.voice.call.as_ref())
                    .is_none_or(|call| call.server == address && call.channel == from)
                    && self.servers[index].is_voice(&to)
                {
                    call::join(self, index, to);
                    let name = (self.servers[index].channel(&to)).map_or("", |c| c.name.as_str());
                    self.toast = Some((t!("voice.moved", channel = name), now + 6.0));
                }
            }
            Update::NoVideo(address, channel) => {
                if let Some(call) = &mut self.voice.call
                    && call.server == address
                    && call.channel == channel
                {
                    call.can_share_video = false;
                }
            }
            Update::LiveOffer(offer) => self.voice.live_files.push(LiveFile {
                id: offer.id,
                chat: offer.chat,
                state: if offer.from.is_some() {
                    LiveState::Offered
                } else {
                    LiveState::Sending
                },
                from: offer.from,
                name: offer.name,
                size: offer.size,
                receivers: Vec::new(),
            }),
            Update::LiveReceiver(id, person, on) => {
                if let Some(file) = self.voice.live_files.iter_mut().find(|f| f.id == id) {
                    file.receivers.retain(|p| *p != person);
                    if on {
                        file.receivers.push(person);
                    }
                }
            }
            Update::LiveProgress(id, event) => {
                if let Some(file) = self.voice.live_files.iter_mut().find(|f| f.id == id) {
                    file.state = match event {
                        LiveFileEvent::Progress { bytes, .. } => LiveState::Saving(bytes),
                        LiveFileEvent::Done => LiveState::Saved,
                        LiveFileEvent::Failed { .. } => LiveState::Failed,
                    };
                }
            }
            Update::CameraStopped => self.voice.camera.reset(),
            Update::Control(view) => self.voice.control.update(view, now),
            Update::Terminal(Feed::Typing(access)) => call::terminal::answered(self, access, now),
            Update::Terminal(feed) => self.voice.terminals.feed(feed),
            Update::TerminalGuests(guests) => self.voice.terminals.guests = guests,
            Update::TerminalStopped => self.voice.terminals.local = None,
            Update::Diagram(feed) => call::diagram::feed(self, feed, now),
            Update::DiagramGuests(guests) => self.voice.diagrams.guests = guests,
            Update::DiagramStopped => call::diagram::stopped(self),
            Update::ControlEnded(sid, why) => call::remote::ended(self, sid, why, now),
            Update::CallVideoStopped(address, channel, sid) => {
                if self
                    .voice
                    .call
                    .as_ref()
                    .is_some_and(|call| call.server == address && call.channel == channel)
                    && let Some(cue) = self.voice.sounds.stop_video(&sid)
                {
                    self.voice.audio.play_cue(cue);
                }
            }
            Update::VoiceDestination(address, id, allowed) => {
                self.voice_destination(&address, id, allowed, now)
            }
            Update::CallRang(address, channel, person) => self.call_rang(&address, channel, person),
            Update::Room(address, channel, peers, sounds) => {
                if let Some(snapshot) = sounds
                    && self.voice.call.as_ref().is_some_and(|call| {
                        call.server == address
                            && call.channel == channel
                            && call.state == crate::server::CallState::Connected
                    })
                {
                    for cue in self.voice.sounds.update(snapshot) {
                        self.voice.audio.play_cue(cue);
                    }
                }
                if let Some(index) = self.index(&address) {
                    let rooms = &mut self.servers[index].voice;
                    if peers.is_empty() {
                        rooms.remove(&channel);
                    } else {
                        rooms.insert(channel, peers);
                    }
                }
            }
            Update::ExternalPhones(address, channel, calls) => {
                if self
                    .voice
                    .call
                    .as_ref()
                    .is_some_and(|call| call.server == address && call.channel == channel)
                {
                    // "Always allow": share with each call once, when it first shows up,
                    // so stopping by hand during a call sticks.
                    if self.voice.audio.settings().phone_always_share
                        && let Some(server) = self.servers.iter().find(|s| s.address == address)
                    {
                        let old = &self.voice.external_phones;
                        for (call, _, status) in &calls {
                            if *status == crate::phone::ConsentStatus::Off
                                && !old.iter().any(|(known, ..)| known == call)
                            {
                                server.send(Command::PhoneConsent(call.clone(), true));
                            }
                        }
                    }
                    self.voice.external_phones = calls;
                }
            }
            Update::SharedAudioStopped(address, channel) => {
                if self
                    .voice
                    .call
                    .as_ref()
                    .is_some_and(|call| call.server == address && call.channel == channel)
                {
                    self.voice.shares.clear();
                    self.voice.stream_audio.clear();
                }
            }
            update @ (Update::AppStopped(_) | Update::StreamAudio(_)) => {
                call::shared_sound(&mut self.voice, update);
            }
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            Update::Phone(generation, event) => {
                call::phone::update(&mut self.voice.phone, generation, event)
            }
            Update::Sharing(on) => {
                // A late result after Stop or leaving doesn't turn the chip back on.
                if self.voice.sharing != call::Sharing::Off || !on {
                    self.voice.sharing = if on {
                        call::Sharing::On
                    } else {
                        self.voice.stop_preview();
                        call::Sharing::Off
                    };
                }
            }
            Update::Stats(address, stats) => {
                if let Some(call) = &mut self.voice.call
                    && call.server == address
                {
                    self.voice.link.push(&stats);
                    call.stats = stats;
                }
            }
            Update::SharedFiles(address, channel, list) => {
                if let Some(index) = self.index(&address) {
                    self.chat(index, channel).shared = Some(list);
                }
            }
            Update::Uploading(address, channel, what) => {
                if let Some(index) = self.index(&address) {
                    self.chat(index, channel).uploading = what;
                }
            }
            Update::Image(address, blob, fetch, frames) => {
                self.files.image(ctx, &address, &blob, fetch, frames)
            }
            Update::LinkCard(id, card) => self.files.link_card(id, card),
            Update::StickerFile(id, picture) => self.files.sticker_loaded(ctx, id, picture),
            Update::Saved(id, saved) => {
                if let Some(notice) = self.files.saved(id, saved) {
                    self.toast = Some((notice, now + 6.0));
                }
            }
            Update::Synced(synced) => {
                self.synced_once = true;
                for known in &synced.servers {
                    if self.index(&known.address).is_none() && !self.left.contains(&known.address) {
                        let commands = self.net.start(known.clone(), None, None);
                        self.servers.push(new_server(
                            known.clone(),
                            t!("home.new_server"),
                            commands,
                        ));
                    }
                }
                // A server being joined is added to the sync state only once signed in.
                let joining = match &self.join {
                    Join::Joining(address) => Some(address.clone()),
                    _ => None,
                };
                for index in (0..self.servers.len()).rev() {
                    let address = &self.servers[index].address;
                    if Some(address) != joining.as_ref()
                        && !synced.servers.iter().any(|known| &known.address == address)
                    {
                        self.remove_server(index);
                    }
                }
                // Deletes from my other devices; mine stay too, as `run` applies them again.
                let synced_deletes = device_sync::dm_deleted(&synced.settings);
                let me = *self.identity.certificate.person_id();
                for id in synced_deletes.ids() {
                    if !self.dm_deleted.contains(id) {
                        for thread in self.threads.values_mut() {
                            thread.deleted(&me, id);
                        }
                    }
                }
                let before = self.dm_deleted.clone();
                self.dm_deleted.merge(&synced_deletes);
                if self.dm_deleted != before {
                    // Saved here only; this came from the sync copy.
                    let data = self.dm_deleted_path.parent().expect("in the data folder");
                    let saved = rusttalk_core::client::save_json(
                        data,
                        &self.dm_deleted_path,
                        &self.dm_deleted,
                    );
                    if let Err(error) = saved {
                        tracing::warn!(error = %format_args!("{error:#}"), "saving deleted DM messages failed");
                    }
                }
                let audio = &self.voice.audio;
                let mut adopted = device_sync::adopt(&audio.settings(), &synced.settings);
                adopted.adopt_pending();
                self.synced = device_sync::synced(&adopted);
                audio.set_settings(adopted);
                self.notify.adopt(&synced.settings);
                self.friends.adopt(&synced.contacts);
                for key in notify::SYNCED {
                    if let Some(value) = synced.settings.get(key) {
                        self.synced.insert(key.into(), value.clone());
                    }
                }
            }
            Update::ForumSent(address, channel, id) => {
                if let Some(server) = self.servers.iter_mut().find(|s| s.address == address)
                    && let Some(chat) = server.chats.get_mut(&channel)
                    && let Some(next) = forum::confirmed(chat, id)
                {
                    forum::send_part(chat, &server.commands, next);
                }
            }
            Update::Unsent(address, draft) => {
                if draft.forum_post {
                    if let Some(index) = self.index(&address)
                        && let Some(chat) = self.servers[index].chats.get_mut(&draft.channel)
                    {
                        forum::failed(chat, draft.id);
                    }
                    return;
                }
                if draft.voice_request.is_some() {
                    self.requests.outgoing.retain(|sent| sent.id != draft.id);
                    return;
                }
                // A poke's note doesn't belong in the composer, where it would go out as text.
                // Nor do votes, or a poll's question.
                let Some(index) =
                    (self.index(&address)).filter(|_| !draft.poke && draft.poll.is_none())
                else {
                    return;
                };
                let me = self.servers[index].dir.me;
                let server = &mut self.servers[index];
                let max = server.max_files;
                let chat = match draft.peer {
                    Some(peer) => &mut self.threads.entry(peer).or_default().chat,
                    None => server.chats.entry(draft.channel).or_default(),
                };
                // Shown in the chat while sending: it stays there, marked failed.
                let Some(draft) = chat.unsent(draft) else {
                    return;
                };
                // Only when nothing new was typed or picked meanwhile, and no other
                // server got this DM.
                let sent = (chat.messages.iter()).any(|msg| msg.id == draft.id && msg.person == me);
                if chat.draft.is_empty() && chat.staged.is_empty() && !sent {
                    chat.open_draft(&server.dir, &draft.text);
                    chat.reply_to = draft.reply_to;
                    chat.editing = draft.edit_of;
                    files::stage(chat, draft.files, max);
                }
            }
            Update::Failed(error) => {
                match &mut self.dialog {
                    Some(Dialog::Invite(invite)) => invite.making = false,
                    Some(Dialog::Leave(_, shown)) => {
                        *shown = Some(error);
                        return;
                    }
                    _ => {}
                }
                self.toast = Some((error, now + 6.0));
            }
        }
    }
}

fn host(address: &url::Url) -> String {
    address.host_str().unwrap_or("server").to_owned()
}

pub fn initials(name: &str) -> String {
    let letters: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".into()
    } else {
        letters
    }
}

/// The presence colour: green, yellow, red (with a bar, see [`ui::presence_dot`]) or grey.
pub fn presence_color(c: &Palette, presence: Presence) -> Color32 {
    match presence {
        Presence::Online => c.success,
        Presence::Away => c.warning,
        Presence::Dnd => c.danger,
        Presence::Offline => c.offline,
    }
}

fn panel_frame(fill: Color32) -> Frame {
    Frame::new()
        .fill(fill)
        .inner_margin(Margin::same(ui::SPACE_S as i8))
}

pub fn show(
    ui: &mut Ui,
    home: &mut Home,
    updates: &mut crate::update::Updater,
    keystore: &mut crate::keystore::Keystore,
) -> Option<Action> {
    let now = ui.input(|input| input.time);
    let focused = ui.input(|input| input.viewport().focused).unwrap_or(true);
    // Window size first: whether a message is being read depends on it.
    home.layout.phone = ui::is_phone(ui.ctx());
    home.layout
        .set_window_width(ui.available_width() - RAIL_WIDTH);
    if home.focused && !focused {
        // Stepping away: read marks reach the other devices and the disk.
        home.publish_settings();
        home.notify.save_soon();
    }
    home.focused = focused;
    while let Ok(update) = home.net.updates.try_recv() {
        home.apply(ui.ctx(), update, now);
    }
    if now >= home.next_backfill && home.backfill.as_ref().is_none_or(|job| job.is_finished()) {
        home.backfill = Some(home.net.backfill(&home.servers, &home.dm_deleted));
        home.next_backfill = now + 3600.0;
    }
    ui.ctx().request_repaint_after(Duration::from_secs(60));
    home.send_queued(ui.ctx(), now);
    while let Some((address, channel)) = home.notify.clicked() {
        home.open_chat(&address, channel);
    }
    if focused {
        for (index, channel) in home.viewing() {
            let address = home.servers[index].address.clone();
            home.notify.mark_read(&address, &channel);
        }
    }
    home.track_activity(ui);
    let settings = home.voice.audio.snapshot();
    ui::apply(ui.ctx(), settings.appearance());
    emoji::set_hover_only(ui.ctx(), settings.emoji_on_hover);
    crate::i18n::set(&settings.language);
    if welcome::show(ui, home) {
        return None;
    }
    switcher::shortcut(ui.ctx(), home);
    admin::take_drops(ui.ctx(), home);
    for (ask, paths) in home.files.answers() {
        if let Ask::Picture = ask {
            if let Some(path) = paths.into_iter().next() {
                match Crop::open(Target::Avatar, &path) {
                    Ok(crop) => home.crop = Some(crop),
                    Err(problem) => home.error = Some(problem.into()),
                }
            }
            continue;
        }
        if let Ask::AttachDm(peer) = ask {
            let max = home.max_files(At::Dm(peer));
            let chat = &mut home.threads.entry(peer).or_default().chat;
            if let Some(problem) = files::stage(chat, paths, max) {
                home.toast = Some((problem, now + 6.0));
            }
            continue;
        }
        let (Ask::Attach(address, _)
        | Ask::PagePictures(address, _)
        | Ask::Share(address, _)
        | Ask::Save(address, _)
        | Ask::Live(address, _)
        | Ask::Icon(address)
        | Ask::Emoji(address, ..)
        | Ask::EmojiZip(address)
        | Ask::Accept(address, ..)) = &ask
        else {
            continue;
        };
        let Some(index) = home.index(address) else {
            continue;
        };
        let server = &mut home.servers[index];
        match ask {
            Ask::Attach(_, channel) | Ask::PagePictures(_, channel) => {
                let max = server.max_files;
                let chat = server.chats.entry(channel).or_default();
                if let Some(problem) = files::stage(chat, paths, max) {
                    home.toast = Some((problem, now + 6.0));
                }
            }
            Ask::Share(_, channel) => {
                if !paths.is_empty() {
                    server.send(Command::ShareFiles(channel, paths));
                }
            }
            Ask::Save(_, file) => {
                if let Some(path) = paths.into_iter().next() {
                    home.files.save(&server.commands, &file, path);
                }
            }
            Ask::Emoji(_, sticker) => admin::stage(home, paths, sticker, ui.ctx()),
            Ask::Icon(address) => {
                let Some(path) = paths.into_iter().next() else {
                    continue;
                };
                match Crop::open(Target::Icon(address), &path) {
                    Ok(crop) => home.crop = Some(crop),
                    Err(problem) => {
                        if let Some(settings) = &mut home.server_settings {
                            settings.apply(AdminUpdate::Failed(problem.into()));
                        }
                    }
                }
            }
            Ask::Live(_, channel) => {
                let Some(path) = paths.into_iter().next() else {
                    continue;
                };
                if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > call::MAX_LIVE_FILE) {
                    home.toast = Some((t!("home.live_file_too_big").into(), now + 6.0));
                } else {
                    server.send(Command::SendLive(channel, path));
                }
            }
            Ask::EmojiZip(_) => {
                if let Some(path) = paths.into_iter().next() {
                    admin::start_import(home, &path, ui.ctx());
                }
            }
            Ask::Picture | Ask::AttachDm(_) => {}
            Ask::Accept(_, id, _) => {
                let Some(path) = paths.into_iter().next() else {
                    continue;
                };
                // Receiving never replaces a file, even one the dialog offered to overwrite.
                if path.exists() {
                    home.toast = Some((t!("home.file_exists").into(), now + 6.0));
                    continue;
                }
                if let Some(file) = home.voice.live_files.iter_mut().find(|f| f.id == id) {
                    file.state = LiveState::Saving(0);
                    server.send(Command::AcceptFile(id, path));
                }
            }
        }
    }
    // A phone had to end while messages were still on their way: say so in a
    // chat, as the toast whenever no other one shows, until this notice was drawn.
    #[cfg(target_os = "android")]
    if home.unsent.is_some()
        && unsent_notice(&mut home.toast, drawn_toast(ui.ctx()), now)
        && let Some(note) = home.unsent.take()
    {
        let _ = std::fs::remove_file(note);
    }
    if home.toast.as_ref().is_some_and(|(_, until)| *until < now) {
        home.toast = None;
    }
    // Always poll route apply work and watch generation changes even when Settings -> Network
    // is closed, so calls/previews/updater react immediately to route switches or expiry.
    let route_applied = home.route_editor.poll(ui.ctx());
    let current_route_gen = rusttalk_core::route::generation();
    if route_applied || home.route_generation != current_route_gen {
        home.route_generation = current_route_gen;
        home.on_route_changed(updates);
    }
    let colors = ui::colors(ui);
    // Above everything, full screen too: who controls this screen, and the prompt.
    call::remote::sharer(ui, home);
    call::terminal::prompt(ui.ctx(), home);
    call::diagram::prompt(ui.ctx(), home);
    if home.voice.stage.full.is_some() {
        CentralPanel::default()
            .frame(panel_frame(colors.main))
            .show(ui, |ui| call::full_view(ui, home));
        // What the call's buttons there open.
        call::screen_dialog(ui.ctx(), home);
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        call::phone::windows(ui.ctx(), home);
        call::terminal::dialog(ui.ctx(), home);
        call::frame(ui.ctx(), home);
        return None;
    }
    // Not in unit tests: route tests change the process-wide route state in parallel, and
    // the banner would shift the layout other tests have already measured.
    if !cfg!(test) && crate::route::status_banner(ui) {
        home.settings = Some(Tab::Network);
    }
    // Joining or leaving a call counts: a restart must never cut one off.
    updates.banner(ui, home.voice.call.is_some());
    view_as::bar(ui, home);
    // Phones: the list is a drawer over what is open. A pick, Back, a swipe from the
    // left edge or a tap beside it moves it.
    let phone = home.layout.phone;
    let area = ui.available_rect_before_wrap();
    let drawer = ui::drawer::Drawer::new(
        ui.ctx(),
        UiId::unique("sidebar drawer"),
        area,
        phone,
        &mut home.layout.list,
    );
    let list = phone && home.layout.list;
    // Fully open: what is open stays unpainted (and unread by screen readers) until
    // the list moves again.
    let covered = phone && drawer.progress >= 1.0;
    let channels = (!phone).then(|| {
        let width = home.layout.channels;
        sidebar_column(ui, home, updates, width)
    });
    let member_server = home.member_server().filter(|_| !list);
    // On phones the member list covers the column, and Back closes it first.
    let members_cover = phone && member_server.is_some() && home.layout.members_shown();
    // Back sits in the open pane's header when it has one (`header_back`), else in
    // a bar of its own; which one is known from the last frame.
    let back_id = phone_back_id();
    let last: PhoneBack = ui.data(|data| data.get_temp(back_id)).unwrap_or_default();
    // Also while the list slides: it stays put under the dimmed edge.
    let wants_back = phone && !covered;
    let in_header = wants_back && !members_cover;
    ui.data_mut(|data| {
        data.insert_temp(
            back_id,
            PhoneBack {
                wanted: in_header,
                ..PhoneBack::default()
            },
        );
    });
    let mut back = false;
    if wants_back && !(in_header && last.in_header) {
        Panel::top("phone-back")
            .frame(panel_frame(colors.side))
            .show(ui, |ui| back = back_button(ui).clicked());
    }
    let mut members = None;
    if let Some(index) = member_server {
        // Toggled from the channel header; the panel only slides.
        let mut shown = home.layout.members_shown();
        members = Panel::right("members")
            .resizable(false)
            .exact_size(if phone {
                ui.available_width()
            } else {
                home.layout.members
            })
            .frame(panel_frame(colors.side))
            .show_collapsible(ui, &mut shown, |ui| member_list(ui, home, index))
            .map(|panel| panel.response.rect);
    }
    if covered {
        CentralPanel::default()
            .frame(Frame::new().fill(colors.main))
            .show(ui, |_| {});
    } else if !members_cover {
        CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(colors.main)
                    .inner_margin(Margin::same(ui::SPACE_M as i8)),
            )
            .show(ui, |ui| match home.view {
                View::Join => join(ui, home),
                View::Find => find(ui, home),
                View::Direct(None) => friends::view(ui, home),
                View::Pokes => pokes::view(ui, home),
                View::Direct(Some(peer)) => dm_pane(ui, home, peer),
                View::Server(index) => {
                    if !show_compatibility(ui, home, index) {
                        return;
                    }
                    match home.servers[index].open {
                        Some(channel) if home.servers[index].is_voice(&channel) => {
                            voice_pane(ui, home, index, channel)
                        }
                        Some(channel) => chat_pane(ui, home, index, channel),
                        None if home.servers[index].events_open
                            && events::enabled(&home.servers[index]) =>
                        {
                            events::pane(ui, home, index)
                        }
                        None => {
                            let server = &home.servers[index];
                            let hint = match &server.status {
                                Status::Stopped(reason) => reason.clone(),
                                Status::Connecting => t!("common.connecting").into(),
                                _ if server.dir.categories.is_empty()
                                    && server.dir.perms.manage_channels =>
                                {
                                    t!("home.no_channels").into()
                                }
                                _ => t!("home.pick_channel").into(),
                            };
                            empty(ui, server.name(), &hint);
                        }
                    }
                }
            });
    }
    let width = ui::drawer::width(area) - RAIL_WIDTH;
    if phone
        && drawer.show(ui.ctx(), |ui| {
            sidebar_column(ui, home, updates, width);
        })
    {
        home.layout.list = false;
    }
    let header: PhoneBack = ui.data(|data| data.get_temp(back_id)).unwrap_or_default();
    if header.in_header != last.in_header {
        ui.ctx()
            .request_discard("Back moved between its bar and the header");
    }
    if back || header.clicked {
        if members_cover {
            home.layout.toggle_members();
        } else {
            home.layout.list = true;
        }
    }
    // Resize handles go last so they sit above the panels on both sides of each edge.
    let mut dragging = false;
    if let Some(channels) = channels.filter(|_| !phone) {
        dragging |= layout::resize_edge(
            ui,
            t!("home.resize_channels"),
            channels,
            true,
            &mut home.layout.channels,
            layout::CHANNELS_WIDTH,
            layout::CHANNELS_RANGE,
        );
    }
    if let Some(members) = members.filter(|_| !phone) {
        dragging |= layout::resize_edge(
            ui,
            t!("home.resize_members"),
            members,
            false,
            &mut home.layout.members,
            layout::MEMBERS_WIDTH,
            layout::MEMBERS_RANGE,
        );
    }
    if !dragging && !home.layout.split_dragging {
        home.layout.save(&home.voice.audio);
    }
    dialogs(ui.ctx(), home);
    focus::dialog(ui.ctx(), home);
    if !voice_requests::popup(ui.ctx(), home) {
        pokes::popup(ui.ctx(), home);
    }
    admin::window(ui.ctx(), home);
    files::viewer(ui.ctx(), &mut home.files);
    call::screen_dialog(ui.ctx(), home);
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    call::phone::windows(ui.ctx(), home);
    call::terminal::dialog(ui.ctx(), home);
    call::frame(ui.ctx(), home);
    let action = crate::settings::show(ui.ctx(), home, updates, keystore);
    updates.dialog(ui.ctx());
    crop_dialog(ui.ctx(), home);
    admin::kick_dialog(ui.ctx(), home);
    switcher::show(ui.ctx(), home);
    action
}

/// One column for rail and channel list, so the bottom panel spans both like
/// Discord. Returns where the channel list went.
fn sidebar_column(
    ui: &mut Ui,
    home: &mut Home,
    updates: &mut crate::update::Updater,
    channels_width: f32,
) -> Rect {
    let colors = ui::colors(ui);
    Panel::left("sidebar")
        .resizable(false)
        .exact_size(RAIL_WIDTH + channels_width)
        .frame(Frame::new())
        .show(ui, |ui| {
            Panel::bottom("you")
                .frame(panel_frame(colors.side))
                .show(ui, |ui| bottom_panel(ui, home, updates));
            Panel::left("servers")
                .resizable(false)
                .exact_size(RAIL_WIDTH)
                .frame(panel_frame(colors.rail))
                .show(ui, |ui| server_rail(ui, home));
            Panel::left("channels")
                .resizable(false)
                .exact_size(channels_width)
                .frame(panel_frame(colors.side))
                .show(ui, |ui| sidebar(ui, home))
                .response
                .rect
        })
        .inner
}

/// Back on a phone: in the open pane's header, or in a bar of its own.
#[derive(Clone, Copy, Default)]
struct PhoneBack {
    /// The pane's header should show it this frame.
    wanted: bool,
    /// A header showed it.
    in_header: bool,
    clicked: bool,
}

fn phone_back_id() -> UiId {
    UiId::unique("phone back")
}

fn back_button(ui: &mut Ui) -> Response {
    let size = Vec2::new(ui::ICON_BUTTON.max(36.0), ui::ICON_BUTTON);
    ui::icon_button(ui, "⬅", t!("common.back"), ui::Look::Ghost, size)
}

/// Phones: Back at the start of a pane's header, so the header and the Back bar
/// take one row together.
fn header_back(ui: &mut Ui) {
    let id = phone_back_id();
    let Some(mut back) = ui.data(|data| data.get_temp::<PhoneBack>(id)) else {
        return;
    };
    if back.wanted {
        back.in_header = true;
        back.clicked |= back_button(ui).clicked();
        ui.data_mut(|data| data.insert_temp(id, back));
    }
}

fn show_compatibility(ui: &mut Ui, home: &mut Home, index: usize) -> bool {
    let compatibility = home.servers[index].compatibility;
    if compatibility_banner(ui, compatibility, crate::settings::PHONE) {
        get_new_app(ui.ctx(), &mut home.settings, crate::settings::PHONE);
    }
    compatibility != Compatibility::AppTooOld
}

/// The "needs a newer app" button: desktops open the updater; phones have none,
/// so they open the download page in the browser.
fn get_new_app(ctx: &egui::Context, page: &mut Option<Tab>, phone: bool) {
    if phone {
        ctx.open_url(egui::OpenUrl::new_tab(crate::settings::DOWNLOAD_PAGE));
    } else {
        *page = Some(Tab::Updates);
    }
}

fn compatibility_banner(ui: &mut Ui, compatibility: Compatibility, phone: bool) -> bool {
    let text = match compatibility {
        Compatibility::Supported => return false,
        Compatibility::ServerTooOld => {
            t!("home.server_too_old")
        }
        Compatibility::AppTooOld => t!("home.app_too_old"),
    };
    let action = if phone {
        t!("home.download_new_app")
    } else {
        t!("home.open_updates")
    };
    ui::card(ui, |ui| {
        ui.add(Label::new(text).wrap());
        compatibility == Compatibility::AppTooOld && ui::primary(ui, action).clicked()
    })
}

fn empty(ui: &mut Ui, title: &str, hint: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.label(ui::heading(title));
        ui.weak(hint);
    });
}

/// Corner radius of a sidebar rail avatar: rounded square at rest, subtly
/// tightened on hover or selection while keeping the same hit target.
fn rail_radius(active: f32) -> f32 {
    ui::avatar_radius(RAIL_BUTTON) - active * 2.0
}

/// Subtle outer border on a hovered or selected rail icon, without shifting layout.
fn paint_rail_outline(
    ui: &Ui,
    rect: Rect,
    radius: f32,
    hover: f32,
    selected: f32,
    is_picture: bool,
) {
    let c = ui::colors(ui);
    let accent = ui::accent(ui);
    let hover_color = c
        .muted
        .lerp_to_gamma(c.strong, 0.35)
        .gamma_multiply(0.75 * hover);
    let border = if is_picture {
        let hover_target = if c.dark { accent.tint } else { accent.strong };
        let picked = accent.base.lerp_to_gamma(hover_target, 0.35 * hover);
        hover_color.lerp_to_gamma(picked, selected)
    } else {
        hover_color.gamma_multiply(1.0 - selected)
    };
    if border.a() > 0 {
        let width = if is_picture {
            1.5 + 0.5 * selected
        } else {
            1.5
        };
        ui.painter().rect_stroke(
            rect.expand(1.5),
            radius + 1.5,
            Stroke::new(width, border),
            StrokeKind::Middle,
        );
    }
}

/// A rounded-square rail button (server initials, DMs or "+") with subtle hover and selection states.
fn rail_button(ui: &mut Ui, text: &str, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(RAIL_BUTTON), Sense::click());
    let c = ui::colors(ui);
    let accent = ui::accent(ui);
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered(),
        ui::HOVER_SECONDS,
    );
    let selected_t =
        ui.ctx()
            .animate_bool_with_time(response.id.with("selected"), selected, ui::STATE_SECONDS);
    let radius = rail_radius(hover.max(selected_t));
    let idle = c.main.lerp_to_gamma(accent.base, 0.16 * hover);
    let picked = accent.base.lerp_to_gamma(ui::ON_FILL, 0.10 * hover);
    let fill = idle.lerp_to_gamma(picked, selected_t);
    let glyph = c
        .text
        .lerp_to_gamma(c.strong, hover)
        .lerp_to_gamma(ui::ON_FILL, selected_t);
    ui.painter().rect_filled(rect, radius, fill);
    paint_rail_outline(ui, rect, radius, hover, selected_t, false);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::new(17.0, ui::bold_family()),
        glyph,
    );
    ui::focus_ring(ui, &response, radius as u8);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn rail_label(button: Response, name: &str, unavailable: bool) -> Response {
    button.widget_info(|| WidgetInfo::labeled(Role::Button, true, name));
    if !unavailable {
        return button.on_hover_text(name);
    }
    let status = t!("home.server_unavailable");
    button
        .ctx
        .accesskit_node_builder(button.id, |node| node.set_description(status));
    let (has_touch, touching) = button
        .ctx
        .input(|i| (i.has_touch_screen(), i.any_touches()));
    if !touching {
        let mut tooltip = if button.has_focus() && !has_touch && !ui::menu_open(&button) {
            egui::Tooltip::for_widget(&button)
        } else {
            egui::Tooltip::for_enabled(&button)
        };
        tooltip.popup = tooltip.popup.align(egui::RectAlign::RIGHT_START);
        tooltip.show(|ui| {
            ui.set_max_width(ui.spacing().tooltip_width);
            ui.label(name);
            ui.weak(status);
        });
    }
    button
}

/// A rounded-square server picture in the sidebar rail, with subtle hover and selection feedback.
fn rail_icon(ui: &mut Ui, icon: &Picture, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(RAIL_BUTTON), Sense::click());
    let c = ui::colors(ui);
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered(),
        ui::HOVER_SECONDS,
    );
    let selected_t =
        ui.ctx()
            .animate_bool_with_time(response.id.with("selected"), selected, ui::STATE_SECONDS);
    let radius = rail_radius(hover.max(selected_t));
    paint_hovered(ui, rect, icon, radius);
    if hover > 0.0 {
        ui.painter()
            .rect_filled(rect, radius, c.strong.gamma_multiply(0.08 * hover));
    }
    paint_rail_outline(ui, rect, radius, hover, selected_t, true);
    ui::focus_ring(ui, &response, radius as u8);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn server_rail(ui: &mut Ui, home: &mut Home) {
    ui.vertical_centered(|ui| {
        ui.add_space(4.0);
        let dms = matches!(home.view, View::Direct(_));
        let button = rail_label(
            rail_button(ui, "💬", dms),
            t!("home.direct_messages"),
            false,
        );
        rail_marks(ui, &button, dms, home.dm_marks());
        if button.clicked() {
            home.view = View::Direct(None);
        }
        // Phones have no Ctrl+K: the quick switcher opens from here.
        if home.layout.phone || ui::TOUCH {
            let search = rail_label(rail_button(ui, "🔍", false), t!("switcher.title"), false);
            if search.clicked() {
                home.switcher.open();
            }
        }
        ui.separator();
        for index in 0..home.servers.len() {
            let marks = home.server_marks(index);
            let server = &home.servers[index];
            let selected = home.view == View::Server(index);
            let button = match &server.dir.icon {
                Some(icon) => rail_icon(ui, icon, selected),
                None => rail_button(ui, &initials(server.name()), selected),
            };
            let unavailable = matches!(server.status, Status::Offline | Status::Stopped(_));
            if unavailable {
                let at = button.rect.right_top() + Vec2::new(-8.0, 8.0);
                ui::unavailable_badge(ui, at, ui::colors(ui).rail);
            }
            let button = rail_label(button, server.name(), unavailable);
            rail_marks(ui, &button, selected, marks);
            if button.clicked() {
                home.view = View::Server(index);
            }
            ui::context_menu(&button, |m| server_menu(m, home, index));
        }
        let adding = matches!(home.view, View::Join | View::Find);
        let plus = rail_label(rail_button(ui, "+", adding), t!("home.add_server"), false);
        ui::menu(&plus, |m| {
            if m.item("🔗", t!("home.join_with_invite")).clicked() {
                home.view = View::Join;
                home.layout.list = false;
            }
            if m.item("🔍", t!("home.find_servers")).clicked() {
                home.view = View::Find;
                home.layout.list = false;
                home.find.open();
            }
        });
    });
}

/// The unread pill beside a rail button (unless it is open) and its mention count.
fn rail_marks(ui: &Ui, button: &Response, selected: bool, (unread, mentions): (bool, u32)) {
    if unread && !selected {
        ui::unread_pill(
            ui,
            button.rect,
            button.rect.left() - ui.max_rect().left() + 6.0,
        );
    }
    if mentions > 0 {
        let at = button.rect.right_bottom() - Vec2::splat(5.0);
        ui::mention_badge(ui, at, mentions, ui::colors(ui).rail);
    }
}

/// The same on a channel or DM row, unless that chat is muted.
/// A DM row counts its copy on every server.
fn chat_marks(ui: &Ui, home: &Home, chats: &[(&url::Url, Id)], dm: bool, row: Rect) {
    paint_marks(ui, row, chat_counts(home, chats, dm));
}

/// Whether the chats are unread and their mentions, leaving out muted ones.
fn chat_counts(home: &Home, chats: &[(&url::Url, Id)], dm: bool) -> (bool, u32) {
    let (prefs, now) = (home.notify.prefs(), notify::unix_now());
    let chats =
        (chats.iter()).filter(|(server, channel)| !prefs.effective(server, channel, dm, now).muted);
    chats.fold((false, 0), |(unread, mentions), (server, channel)| {
        (
            unread || home.notify.unread(server, channel),
            mentions + home.notify.mentions(server, channel),
        )
    })
}

fn paint_marks(ui: &Ui, row: Rect, (unread, mentions): (bool, u32)) {
    if unread {
        ui::unread_pill(ui, row, 4.0);
    }
    if mentions > 0 {
        let at = egui::pos2(row.right() - 16.0, row.center().y);
        ui::mention_badge(ui, at, mentions, ui::colors(ui).side);
    }
}

fn bottom_panel(ui: &mut Ui, home: &mut Home, updates: &mut crate::update::Updater) {
    if updates.available() {
        Frame::new()
            .inner_margin(Margin::symmetric(0, ui::SPACE_XS as i8))
            .show(ui, |ui| updates.button(ui));
    }
    call::call_bar(ui, home);
    user_bar::show(ui, home, updates.available());
}

fn sidebar(ui: &mut Ui, home: &mut Home) {
    match home.view {
        View::Server(index) => channel_list(ui, home, index),
        _ => dm_list(ui, home),
    }
}

/// The crop step, over the settings that opened it; Save uploads, Cancel keeps the
/// current picture.
fn crop_dialog(ctx: &egui::Context, home: &mut Home) {
    let Some(crop) = &mut home.crop else {
        return;
    };
    let bytes = match crop.show(ctx) {
        Outcome::Open => return,
        Outcome::Cancel => None,
        Outcome::Save(bytes) => Some(bytes),
    };
    let Some(crop) = home.crop.take() else {
        return;
    };
    let Some(bytes) = bytes else {
        return;
    };
    match crop.target {
        Target::Avatar => {
            home.net.set_avatar(
                bytes,
                home.servers.iter().map(|s| s.commands.clone()).collect(),
            );
        }
        Target::Icon(address) => {
            if let Some(index) = home.index(&address) {
                home.servers[index].send(Command::Admin(Admin::SetIcon(bytes)));
            }
        }
        // Back to the list of picked pictures, to upload with the others.
        Target::Emoji { slot, .. } => admin::cropped(home, slot, bytes),
    }
}

/// Which frame of a looping animation shows `now` ms in, and the ms until the next one.
/// `None` when the delays add up to nothing.
pub fn frame_at(delays: impl Iterator<Item = u32> + Clone, now: u64) -> Option<(usize, u64)> {
    let total: u64 = delays.clone().map(u64::from).sum();
    let mut at = now.checked_rem(total)?;
    for (index, ms) in delays.enumerate() {
        let ms = u64::from(ms);
        if at < ms {
            return Some((index, ms - at));
        }
        at -= ms;
    }
    None
}

/// Paint a profile picture or server icon in `rect`: its first frame, and it plays
/// only while the pointer is over it, so lists of them cost nothing at rest.
pub fn paint_hovered(ui: &Ui, rect: Rect, picture: &Picture, radius: f32) {
    paint_face(ui, rect, picture, radius, false);
}

/// Paint a picture in `rect`, playing while `play` (a hovered tile, say).
pub fn paint_playing(ui: &Ui, rect: Rect, picture: &Picture, radius: f32, play: bool) {
    egui::Image::new(&frame_when(ui, picture, play))
        .corner_radius(radius)
        .paint_at(ui, rect);
}

/// `paint_hovered` that also plays while the person speaks in a call, like Discord,
/// unless animations are set to play only on hover.
fn paint_face(ui: &Ui, rect: Rect, picture: &Picture, radius: f32, speaking: bool) {
    let play = ui.rect_contains_pointer(rect)
        || (speaking && ui.is_rect_visible(rect) && !emoji::hover_only(ui.ctx()));
    egui::Image::new(&frame_when(ui, picture, play))
        .corner_radius(radius)
        .paint_at(ui, rect);
}

/// A server emoji or sticker in a square of `size`. It plays while on screen in a
/// focused window, or only under the pointer with "Animated emoji: On hover".
pub fn emoji_image(ui: &mut Ui, picture: &Picture, size: f32, label: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let play = ui.is_rect_visible(rect)
        && ui.input(|input| input.focused)
        && (!emoji::hover_only(ui.ctx()) || response.hovered());
    egui::Image::new(&frame_when(ui, picture, play)).paint_at(ui, rect);
    response.widget_info(|| WidgetInfo::labeled(Role::Image, true, label));
    response
}

/// Paint a server emoji or sticker in `rect`, playing like `emoji_image`: all the time
/// while on screen in a focused window, or only under the pointer with "On hover".
pub fn paint_live(ui: &Ui, rect: Rect, picture: &Picture, radius: f32) {
    let play = ui.is_rect_visible(rect)
        && ui.input(|input| input.focused)
        && (!emoji::hover_only(ui.ctx()) || ui.rect_contains_pointer(rect));
    paint_playing(ui, rect, picture, radius, play);
}

/// A picture's frames on the GPU and how long each shows, in ms.
type Frames = Arc<(Vec<TextureHandle>, Vec<u32>)>;

/// Drop the textures of a picture, for one that is no longer kept.
pub fn forget_textures(ctx: &egui::Context, key: &[u8; 32]) {
    ctx.data_mut(|data| {
        data.remove::<TextureHandle>(UiId::unique(("picture", *key)));
        data.remove::<Frames>(UiId::unique(("picture frames", *key)));
    });
}

/// The texture for a picture's current frame, or its first with `play` false (and no
/// repaints). Textures are shared by every use of the same image file; the other
/// frames only go to the GPU once something plays, and only their delays stay in RAM.
// shortcut: textures stay in egui's memory until restart; evict them if people change pictures often.
fn frame_when(ui: &Ui, picture: &Picture, play: bool) -> TextureHandle {
    let load = |image: ColorImage| {
        ui.ctx()
            .load_texture("picture", image, TextureOptions::LINEAR)
    };
    let first_id = UiId::unique(("picture", picture.key));
    let first = ui.data(|data| data.get_temp::<TextureHandle>(first_id));
    let first = first.unwrap_or_else(|| {
        let made = load(picture.first.0.clone());
        ui.data_mut(|data| data.insert_temp(first_id, made.clone()));
        made
    });
    if !play {
        return first;
    }
    let all_id = UiId::unique(("picture frames", picture.key));
    let all = match ui.data(|data| data.get_temp::<Frames>(all_id)) {
        Some(all) => all,
        None => {
            // The first frame shows while the others decode in the background.
            let repaint = ui.ctx().clone();
            let Some(rest) = picture.rest(move || repaint.request_repaint()) else {
                return first;
            };
            if rest.is_empty() {
                return first;
            }
            let delays = std::iter::once(picture.first.1)
                .chain(rest.iter().map(|(_, ms)| *ms))
                .collect();
            let rest = rest.into_iter().map(|(image, _)| load(image));
            let made: Frames =
                Arc::new((std::iter::once(first.clone()).chain(rest).collect(), delays));
            ui.data_mut(|data| data.insert_temp(all_id, made.clone()));
            made
        }
    };
    let now = (ui.input(|input| input.time) * 1000.0) as u64;
    match frame_at(all.1.iter().copied(), now) {
        Some((at, left)) => {
            ui.ctx().request_repaint_after(Duration::from_millis(left));
            all.0[at].clone()
        }
        None => first,
    }
}

fn dm_list(ui: &mut Ui, home: &mut Home) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(ui::SPACE_S);
        ui.label(ui::title(t!("home.direct_messages")));
    });
    ui.separator();
    let waiting = (home.friends.contacts())
        .filter(|(_, contact)| contact.relation == Relation::Incoming)
        .count();
    let friends = home.view == View::Direct(None);
    let label = t!("friends.title");
    let row = ui::nav_row(ui, "👥", label, friends, Role::Button, label);
    if waiting > 0 {
        let at = egui::pos2(row.rect.right() - 16.0, row.rect.center().y);
        ui::mention_badge(ui, at, waiting as u32, ui::colors(ui).side);
    }
    if row.clicked() {
        home.view = View::Direct(None);
        home.layout.list = false;
    }
    pokes::nav_row(ui, home);
    ui.add_space(ui::SPACE_XS);
    let (shown, requests) = home.friends.split(&home.dm_peers(), home.friends_only());
    ScrollArea::vertical().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for peer in &shown {
            dm_row(ui, home, peer, false);
        }
        if !requests.is_empty() {
            CollapsingHeader::new(t!("friends.message_requests", number = requests.len()))
                .id_salt("message-requests")
                .default_open(false)
                .show(ui, |ui| {
                    for peer in &requests {
                        dm_row(ui, home, peer, true);
                    }
                });
        }
    });
    if shown.is_empty() && requests.is_empty() {
        ui.weak(t!("home.no_chats"));
    }
}

/// A DM in the list: picture, name and the servers it goes over. A message request
/// stays quiet: no unread marks.
fn dm_row(ui: &mut Ui, home: &mut Home, peer: &Id, request: bool) {
    let none = Directory::default();
    let selected = home.view == View::Direct(Some(*peer));
    let name = home.peer_name(peer);
    let dir = home.dm_server(peer).map_or(&none, |i| &home.servers[i].dir);
    let caption = home.shared_names(peer);
    if selected {
        let rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 44.0));
        ui.painter()
            .rect_filled(rect, ui::RADIUS_ROW, ui::colors(ui).selected);
    }
    let marks = if request {
        (false, 0)
    } else {
        let replicas = home.replicas(peer);
        let chats: Vec<_> = (replicas.iter())
            .map(|(index, channel)| (&home.servers[*index].address, *channel))
            .collect();
        chat_counts(home, &chats, true)
    };
    let row = ui::row_button(ui, 44.0, &name, |ui| {
        face(ui, dir, peer, &name, 30.0, true);
        // The text ends before the mention badge instead of running under it.
        let badge = if marks.1 > 0 { 24.0 } else { 0.0 };
        ui.vertical(|ui| {
            ui.set_max_width(ui.available_width() - badge);
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add(Label::new(ui::bold(&name)).truncate());
            ui.add(Label::new(ui::caption(&caption)).truncate());
        });
    });
    paint_marks(ui, row.rect, marks);
    if row.clicked() {
        home.view = View::Direct(Some(*peer));
        home.layout.list = false;
    }
}

/// The top of the channel list: the server name and a chevron on one row, all of it
/// a button that opens the server menu below it.
fn server_header(ui: &mut Ui, name: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), SERVER_HEADER),
        Sense::click(),
    );
    let c = ui::colors(ui);
    let open = ui::menu_open(&response);
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered() || open,
        ui::HOVER_SECONDS,
    );
    let turn = ui
        .ctx()
        .animate_bool_with_time(response.id.with("open"), open, ui::STATE_SECONDS);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        ui::RADIUS_ROW,
        Color32::TRANSPARENT.lerp_to_gamma(c.hover_row(), hover),
    );
    ui::focus_ring(ui, &response, ui::RADIUS_ROW);
    let text = c.text.lerp_to_gamma(c.strong, hover);
    let galley = painter.layout(
        name.to_owned(),
        FontId::new(ui::TEXT_TITLE - 1.0, emoji::family(true)),
        c.strong,
        (rect.width() - 44.0).max(0.0),
    );
    let at = egui::pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0);
    painter.galley(at, galley, c.strong);
    // A chevron that turns up while the menu is open.
    let center = egui::pos2(rect.right() - 18.0, rect.center().y);
    let (w, h) = (5.0, 3.0 * (1.0 - 2.0 * turn));
    painter.line(
        vec![
            center + Vec2::new(-w, -h),
            center + Vec2::new(0.0, h),
            center + Vec2::new(w, -h),
        ],
        egui::Stroke::new(1.8, text),
    );
    let label = t!("home.server_menu", name = name);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn channel_list(ui: &mut Ui, home: &mut Home, index: usize) {
    let server = &home.servers[index];
    let perms = server.dir.perms;
    let online = server.status == Status::Online;
    let header = server_header(ui, server.name());
    ui::menu_below(&header, |m| server_menu(m, home, index));
    ui.add_space(2.0);
    let server = &home.servers[index];
    match &server.status {
        Status::Connecting => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(t!("common.connecting"));
            });
        }
        Status::Offline => {
            ui.weak(t!("home.offline"));
        }
        Status::Stopped(reason) => {
            if server.compatibility != Compatibility::AppTooOld {
                error_text(ui, Some(reason));
            }
            if ui::secondary(ui, t!("home.remove_server")).clicked() {
                home.leave_server(index);
                return;
            }
        }
        Status::Online => {}
    }
    if online && perms.owner && server.dir.name.is_none() {
        ui.add_space(6.0);
        if ui::primary(ui, t!("home.name_your_server")).clicked() {
            home.dialog = Some(Dialog::ServerName(index, String::new()));
        }
    }
    ui.separator();
    let mut pick = None;
    let mut add = None;
    let mut talk = None;
    let mut person_pick = None;
    let mut moved = None;
    let mut channel_edit = None;
    let mut category_dialog = None;
    let mut rule_change = None;
    let mut focus_pick = None;
    let now = notify::unix_now();
    let all = &server.dir.categories;
    // People who manage channels see empty categories too, so they can fill or delete
    // them; members (and "View server as") see only categories with a channel.
    let shown = reorder::shown(all, perms.manage_channels && !perms.preview);
    let categories = &*shown;
    let can_manage = perms.manage_channels && online && !perms.preview;
    let can_move = can_manage && !server.order_unavailable && reorder::order(all).is_some();
    let mut show_events = false;
    if events::enabled(server) {
        let selected = server.open.is_none() && server.events_open;
        show_events = events::nav_row(ui, server, selected);
    }
    // On phones, drag scrolls the list; the hold menu still offers every move.
    let can_drag = can_move && !ui::TOUCH;
    let area = ScrollArea::vertical().animated(false);
    area.show_viewport(ui, |ui, viewport| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let list_id = UiId::unique(("channel order", &server.address));
        let mut list = reorder::List::begin(ui, list_id, categories, can_drag);
        for category in categories {
            let block = category.id.unwrap_or_default();
            if let Some(id) = category.id.filter(|_| !category.name.is_empty()) {
                let item = reorder::Item::Category(id);
                let header = list.row(ui, item, block, |ui| {
                    ui.add_space(ui::SPACE_S);
                    ui.horizontal(|ui| {
                        ui.add_space(ui::SPACE_XS);
                        let label = ui::caption_label(ui, &category.name);
                        if perms.manage_channels && !perms.preview {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let plus = ui::icon_button(
                                    ui,
                                    "+",
                                    &t!("home.create_channel_in", category = category.name),
                                    Look::Ghost,
                                    Vec2::splat(22.0),
                                );
                                if plus.clicked() {
                                    add = Some(category.id);
                                }
                            });
                        }
                        label
                    })
                    .inner
                });
                if can_manage {
                    let header = if can_drag {
                        header
                    } else {
                        header.interact(Sense::click())
                    };
                    ui::context_menu(&header, |m| {
                        if m.item("✏", t!("home.rename_category")).clicked() {
                            category_dialog =
                                Some(Dialog::RenameCategory(index, id, category.name.clone()));
                        }
                        if can_move {
                            moved = moved.or(reorder::menu(m, categories, item));
                        }
                        m.separator();
                        if m.danger("🗑", t!("home.delete_category")).clicked() {
                            category_dialog =
                                Some(Dialog::DeleteItem(index, item, category.name.clone()));
                        }
                    });
                }
            }
            for channel in &category.channels {
                let item = reorder::Item::Channel(channel.id);
                let selected = server.open == Some(channel.id);
                let button = list.row(ui, item, block, |ui| {
                    let icon = channel_icon(channel);
                    let locked = limited(server, channel);
                    let mut label = format!("{icon}  {}", channel.name);
                    if locked {
                        label = format!("{label}, {}", t!("home.channel_limited"));
                    }
                    let focus = channel.focus(now).is_some_and(|until| {
                        // Redraw when it ends, so the badge goes.
                        let left = std::time::Duration::from_secs((until - now) as u64);
                        ui.ctx().request_repaint_after(left);
                        true
                    });
                    // Leave room for the Join button, member count and Focus badge.
                    let limited = channel.voice && channel.member_limit > 0;
                    let trailing = match (channel.voice, limited) {
                        (true, true) => call::JOIN_SLOT + call::COUNT_SLOT,
                        (true, false) => call::JOIN_SLOT,
                        _ => 0.0,
                    } + if focus { focus::badge_room(ui) } else { 0.0 };
                    let row = ui
                        .add_enabled_ui(online || !channel.voice, |ui| {
                            let (name, role) = (&channel.name, Role::Button);
                            ui::nav_row_locked(
                                ui, icon, name, selected, role, &label, trailing, locked,
                            )
                        })
                        .inner;
                    let lock = if locked { ui::NAV_LOCK_WIDTH } else { 0.0 };
                    if focus {
                        focus::badge(ui, row.rect, &channel.name, selected, trailing + lock);
                    }
                    let chats = [(&server.address, channel.id)];
                    let marks = row.rect.with_max_x(row.rect.right() - trailing - lock);
                    chat_marks(ui, home, &chats, false, marks);
                    if limited {
                        call::member_count(ui, server, &channel.id, channel.member_limit, &row);
                    }
                    if channel.voice {
                        if call::join_button(ui, home, server, &channel.id, &row, selected) {
                            talk = Some(channel.id);
                        }
                        if let Some(picked) = call::room_list(ui, home, server, &channel.id) {
                            person_pick = Some(picked);
                        }
                    }
                    row
                });
                // A click only opens the chat; a voice channel is joined with its button or
                // a double-click.
                if button.clicked() {
                    pick = Some(channel.id);
                }
                if channel.voice && online && button.double_clicked() {
                    talk = Some(channel.id);
                }
                ui::context_menu(&button, |m| {
                    let rule = home.notify.prefs().channel(&channel.id);
                    if let Some(rule) = notify::rule_menu(m, &rule, true, notify::unix_now()) {
                        rule_change = Some((channel.id, rule));
                    }
                    if perms.start_focus && online && channel.voice && !perms.preview {
                        m.separator();
                        if let Some(start) = focus::menu(m, channel) {
                            focus_pick = Some((channel.id, start));
                        }
                    }
                    if can_manage {
                        m.separator();
                        if m.item("✏", t!("home.edit_channel")).clicked() {
                            channel_edit = Some((channel.id, ChannelEdit::Rename));
                        }
                        if can_move {
                            moved = moved.or(reorder::menu(m, categories, item));
                        }
                        m.separator();
                        if m.danger("🗑", t!("home.delete_channel")).clicked() {
                            channel_edit = Some((channel.id, ChannelEdit::Delete));
                        }
                    }
                });
            }
        }
        moved = moved.or(list.end(ui, categories, viewport));
    });
    let moved = moved.map(|(item, slot)| (item, reorder::widen(categories, all, item, slot)));
    if let Some((item, slot)) = moved {
        let server = &mut home.servers[index];
        if reorder::place(&mut server.dir.categories, item, slot)
            && let Some(order) = reorder::order(&server.dir.categories)
        {
            server.send(Command::Reorder(order));
        }
    }
    if let Some((channel, rule)) = rule_change {
        home.set_rule(index, Some(channel), rule);
    }
    if let Some((id, edit)) = channel_edit {
        home.edit_channel(index, id, edit);
    }
    match focus_pick {
        Some((id, true)) => home.focus = Some(focus::Dialog::start(index, id)),
        Some((id, false)) => home.servers[index].send(Command::SetFocus(id, 0)),
        None => {}
    }
    if category_dialog.is_some() {
        home.dialog = category_dialog;
    }
    if show_events {
        let server = &mut home.servers[index];
        server.open = None;
        server.events_open = true;
        home.layout.list = false;
    }
    if let Some(channel) = pick {
        home.servers[index].open = Some(channel);
        home.layout.list = false;
    }
    if let Some(channel) = talk {
        call::join(home, index, channel);
    }
    if let Some((person, picked)) = person_pick {
        let now = ui.input(|input| input.time);
        person::apply(home, index, person, picked, now);
    }
    if let Some(category) = add {
        home.dialog = Some(new_channel(&home.servers[index], index, category));
    }
}

/// The server header menu, with only what this person may do.
fn server_menu(m: &mut ui::Menu, home: &mut Home, index: usize) {
    let perms = home.servers[index].dir.perms;
    // Previewing: what those roles would find here, but none of it works.
    if perms.preview {
        m.caption(t!("view_as.menu_off"));
        m.ui().disable();
    }
    if perms.invite && m.item("👤", t!("home.invite_people")).clicked() {
        home.dialog = Some(Dialog::Invite(Invite {
            server: index,
            uses: 10,
            seconds: DAY * 7,
            making: false,
            link: None,
        }));
    }
    if m.item("⚙", t!("home.server_settings")).clicked() {
        admin::open(home, index, admin::Tab::Members);
    }
    if perms.manage_channels || perms.manage_server {
        m.separator();
        if perms.manage_channels {
            if m.item("#", t!("home.create_channel")).clicked() {
                home.dialog = Some(new_channel(&home.servers[index], index, None));
            }
            if m.item("🗀", t!("home.create_category")).clicked() {
                home.dialog = Some(Dialog::Category(index, String::new()));
            }
        }
        if perms.manage_server && m.item("✏", t!("home.rename_server")).clicked() {
            let name = home.servers[index].dir.name.clone().unwrap_or_default();
            home.dialog = Some(Dialog::ServerName(index, name));
        }
    }
    m.separator();
    let rule = home.notify.prefs().server(&home.servers[index].address);
    if let Some(rule) = notify::rule_menu(m, &rule, false, notify::unix_now()) {
        home.set_rule(index, None, rule);
    }
    m.separator();
    if m.item("🏷", t!("home.change_my_name")).clicked() {
        let server = &home.servers[index];
        let mine = server
            .dir
            .members
            .iter()
            .find(|m| m.person == server.dir.me);
        home.dialog = Some(Dialog::MyName(
            index,
            mine.and_then(|m| m.name.clone()).unwrap_or_default(),
        ));
    }
    m.separator();
    if m.danger("🚪", t!("home.leave_server")).clicked() {
        home.dialog = Some(Dialog::Leave(index, None));
    }
}

fn new_channel(server: &Server, index: usize, category: Option<Id>) -> Dialog {
    Dialog::Channel {
        server: index,
        name: String::new(),
        kind: ChannelKind::Text,
        category: category.or_else(|| default_category(server, ChannelKind::Text)),
        new_category: category_name(ChannelKind::Text).into(),
    }
}

/// A category that already holds this kind of channel, else an empty one;
/// `None` makes a new category named after the kind.
/// Pages go with the text channels; only voice has a category of its own.
fn default_category(server: &Server, kind: ChannelKind) -> Option<Id> {
    let voice = kind == ChannelKind::Voice;
    let categories = || server.dir.categories.iter().filter(|c| c.id.is_some());
    categories()
        .find(|c| c.channels.iter().any(|channel| channel.voice == voice))
        .or_else(|| categories().find(|c| c.channels.is_empty()))
        .and_then(|c| c.id)
}

fn category_name(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Voice => t!("home.voice_channels"),
        ChannelKind::Text | ChannelKind::Page | ChannelKind::Forum => t!("home.text_channels"),
    }
}

/// Only some roles can see this channel: the role everyone gets can't read it.
fn limited(server: &Server, channel: &Channel) -> bool {
    let everyone = &server.dir.default_role;
    !everyone.is_empty() && admin::channel_role_flags(channel, everyone) & 1 == 0
}

/// The glyph before a channel's name.
pub fn channel_icon(channel: &Channel) -> &'static str {
    kind_icon(ChannelKind::of(channel))
}

fn kind_icon(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Text => "#",
        ChannelKind::Voice => "🔊",
        ChannelKind::Page => "📄",
        ChannelKind::Forum => "💬",
    }
}

pub fn avatar(ui: &mut Ui, person: &Id, name: &str, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint_avatar(ui, rect, person, name);
    response
}

fn paint_avatar(ui: &Ui, rect: Rect, person: &Id, name: &str) {
    let size = rect.width();
    let color: Color32 = Hsva::new(f32::from(person[0]) / 255.0, 0.45, 0.6, 1.0).into();
    ui.painter()
        .rect_filled(rect, ui::avatar_radius(size), color);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        initials(name),
        FontId::new(size * 0.38, ui::bold_family()),
        ui::ON_FILL,
    );
}

fn avatar_atom(member: &Member, name: &str) -> egui::Atom<'static> {
    let size = ui::SPACE_XL;
    if let Some(picture) = &member.avatar {
        let picture = picture.clone();
        return egui::Atom::paint(Vec2::splat(size), move |ui, args| {
            paint_hovered(ui, args.rect, &picture, ui::avatar_radius(size));
        });
    }
    let person = member.person;
    let name = name.to_owned();
    egui::Atom::paint(Vec2::splat(size), move |ui, args| {
        paint_avatar(ui, args.rect, &person, &name);
    })
}

/// A member's picture (or initials) and, with `dot`, their presence: green, yellow or grey.
/// It only senses hover, so a click on it reaches the row or tile around it.
pub fn face(
    ui: &mut Ui,
    dir: &Directory,
    person: &Id,
    name: &str,
    size: f32,
    dot: bool,
) -> Response {
    face_speaking(ui, dir, person, name, size, dot, false)
}

/// `face` with an optional speaking outline painted beneath any presence dot.
pub fn face_speaking(
    ui: &mut Ui,
    dir: &Directory,
    person: &Id,
    name: &str,
    size: f32,
    dot: bool,
    speaking: bool,
) -> Response {
    let member = dir.members.get(person);
    member_face_speaking(ui, member, person, name, size, dot, speaking)
}

/// `face` for a member already looked up (`None` when not a member).
fn member_face(
    ui: &mut Ui,
    member: Option<&Member>,
    person: &Id,
    name: &str,
    size: f32,
    dot: bool,
) -> Response {
    member_face_speaking(ui, member, person, name, size, dot, false)
}

fn member_face_speaking(
    ui: &mut Ui,
    member: Option<&Member>,
    person: &Id,
    name: &str,
    size: f32,
    dot: bool,
    speaking: bool,
) -> Response {
    let response = match member.and_then(|m| m.avatar.as_ref()) {
        Some(picture) => {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
            paint_face(ui, rect, picture, ui::avatar_radius(size), speaking);
            response
        }
        None => avatar(ui, person, name, size),
    };
    if speaking {
        ui::speaking_outline(ui, response.rect);
    }
    if dot {
        let c = ui::colors(ui);
        let presence = member.map_or(Presence::Offline, |m| m.presence);
        let radius = (size * 0.16).max(4.0);
        let color = presence_color(c, presence);
        let barred = presence == Presence::Dnd;
        ui::presence_dot(ui, response.rect, radius, color, c.side, barred);
    }
    response
}

/// The member list's grouping: who is listed, under which heading, in display order.
/// Kept between frames and rebuilt only when the members (their presence too), the
/// separately listed roles or who this is change, so a big server isn't regrouped or
/// re-sorted every frame.
struct MemberGroups {
    generation: u64,
    me: Id,
    separate: Vec<String>,
    listed: usize,
    /// Each heading's role (`None` for everyone else) and its members' places in the list.
    groups: Vec<(Option<String>, Vec<usize>)>,
}

impl MemberGroups {
    fn of(dir: &Directory) -> Self {
        let separate: Vec<String> = (dir.roles.iter())
            .filter(|role| role.separate)
            .map(|role| role.id.clone())
            .collect();
        // Roles shown separately get their own heading, highest first; everyone else after.
        let mut groups: Vec<(Option<String>, Vec<usize>)> = (separate.iter())
            .map(|id| (Some(id.clone()), Vec::new()))
            .chain([(None, Vec::new())])
            .collect();
        let mut listed = 0;
        for (at, member) in dir.members.iter().enumerate() {
            // You always see yourself, also while invisible.
            if !member.listed() && member.person != dir.me {
                continue;
            }
            listed += 1;
            let group = dir
                .heading_role(&member.roles)
                .and_then(|role| separate.iter().position(|id| *id == role.id))
                .unwrap_or(separate.len());
            groups[group].1.push(at);
        }
        // Online first, then Do not disturb, Away and Offline; by name within each.
        for (_, list) in &mut groups {
            list.sort_by_cached_key(|&at| {
                let member = &dir.members[at];
                let status = match member.presence {
                    Presence::Online => 0,
                    Presence::Dnd => 1,
                    Presence::Away => 2,
                    Presence::Offline => 3,
                };
                (status, member.display_name().to_lowercase())
            });
        }
        MemberGroups {
            generation: dir.members.generation(),
            me: dir.me,
            separate,
            listed,
            groups,
        }
    }

    fn current(&self, dir: &Directory) -> bool {
        self.generation == dir.members.generation()
            && self.me == dir.me
            && (self.separate.iter()).eq((dir.roles.iter()).filter(|r| r.separate).map(|r| &r.id))
    }

    /// The cached grouping for `dir`, regrouped first if it changed.
    fn cached(ctx: &egui::Context, dir: &Directory) -> Arc<Self> {
        let id = UiId::unique("member groups");
        let cached = ctx.data(|d| d.get_temp::<Arc<Self>>(id));
        match cached {
            Some(groups) if groups.current(dir) => groups,
            _ => {
                let groups = Arc::new(Self::of(dir));
                ctx.data_mut(|d| d.insert_temp(id, groups.clone()));
                groups
            }
        }
    }
}

fn member_list(ui: &mut Ui, home: &mut Home, index: usize) {
    const ROW: f32 = 40.0;
    let server = &home.servers[index];
    let dir = &server.dir;
    let grouped = MemberGroups::cached(ui.ctx(), dir);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(ui::SPACE_XS);
        ui::caption_label(ui, &t!("home.members_count", count = grouped.listed));
    });
    let mut pick = None;
    let headings = grouped.groups.len() > 1;
    ScrollArea::vertical().show_viewport(ui, |ui, viewport| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let origin = ui.max_rect().top();
        // Rows are all the same height, so only those in view are built; the rest is space.
        let step = ROW + ui.spacing().item_spacing.y;
        for (id, list) in grouped.groups.iter().filter(|(_, list)| !list.is_empty()) {
            if headings {
                let caption = id
                    .as_ref()
                    .and_then(|id| dir.roles.iter().find(|role| role.id == *id))
                    .map_or_else(
                        || t!("home.members_count", count = list.len()),
                        |role| t!("home.role_count", name = role.name, count = list.len()),
                    );
                ui.horizontal(|ui| {
                    ui.add_space(ui::SPACE_XS);
                    ui::section(ui, &caption);
                });
            }
            let top = ui.cursor().top() - origin;
            let first = (((viewport.min.y - top) / step).floor().max(0.0) as usize).min(list.len());
            let last =
                (((viewport.max.y - top) / step).ceil().max(0.0) as usize).clamp(first, list.len());
            ui.add_space(first as f32 * step);
            for member in list[first..last].iter().map(|&at| &dir.members[at]) {
                let name = member.display_name();
                let me = member.person == dir.me;
                let label = if me {
                    t!("home.name_you", name = name)
                } else {
                    name.clone()
                };
                let row = ui::row_button(ui, ROW, &label, |ui| {
                    member_face(ui, Some(member), &member.person, &name, 30.0, true);
                    let text = if member.presence == Presence::Offline {
                        emoji::text(&label).color(ui::colors(ui).muted)
                    } else if let Some(color) = dir.name_color(&member.roles) {
                        emoji::text(&label).color(color)
                    } else {
                        emoji::text(&label)
                    };
                    ui.add(Label::new(text).truncate());
                });
                let target = person::Person::of(home, server, member.person);
                if let Some(picked) = person::interact(ui, &row, &target, false) {
                    pick = Some((member.person, picked));
                }
            }
            ui.add_space((list.len() - last) as f32 * step);
        }
    });
    if let Some((person, picked)) = pick {
        let now = ui.input(|input| input.time);
        person::apply(home, index, person, picked, now);
    }
}

fn join(ui: &mut Ui, home: &mut Home) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.2);
        ui::max_width(ui, 440.0);
        match &mut home.join {
            Join::Idle(error) => {
                // A device restored from the words knows no servers until it signs in to one,
                // whose synced list brings back the rest.
                let empty = home.servers.is_empty();
                if empty && home.restored {
                    ui.label(ui::heading(t!("home.find_yours")));
                    ui.label(t!("home.find_yours_help"));
                } else {
                    ui.label(ui::heading(if empty {
                        t!("home.no_servers")
                    } else {
                        t!("home.join_server")
                    }));
                    ui.label(t!("home.paste_invite"));
                    if empty {
                        ui.label(ui::caption(t!("home.other_device_hint")));
                    }
                }
                ui.add_space(12.0);
                let label = ui.label(t!("home.invite_or_address"));
                let field = ui
                    .add(
                        ui::field(&mut home.invite)
                            .hint_text("rusttalk://…")
                            .desired_width(400.0),
                    )
                    .labelled_by(label.id);
                error_text(ui, error.as_deref());
                if ui::primary_large(ui, t!("home.continue")).clicked() || submitted(ui, &field) {
                    let text = home.invite.trim();
                    // Already joined here: no membership check, which would replace the live session.
                    if let Ok(link) = InviteLink::parse(text) {
                        let probe = home.index(&link.server.address).is_none();
                        home.net.preview(link, probe);
                        home.join = Join::Checking;
                    } else if let Some(address) = server_address(text) {
                        let probe = home.index(&address).is_none();
                        home.net.check_address(address, probe);
                        home.join = Join::Checking;
                    } else {
                        tracing::warn!("invite link or address rejected");
                        *error_slot(&mut home.join) = Some(t!("home.not_invite").into());
                    }
                }
                official_servers(ui, home);
            }
            Join::Checking => {
                ui.spinner();
                ui.label(t!("home.checking_server"));
            }
            Join::UpdateRequired => {
                if compatibility_banner(ui, Compatibility::AppTooOld, crate::settings::PHONE) {
                    get_new_app(ui.ctx(), &mut home.settings, crate::settings::PHONE);
                }
                if ui::secondary_large(ui, t!("common.back")).clicked() {
                    home.join = Join::Idle(None);
                }
            }
            Join::Joining(_) => {
                ui.spinner();
                ui.label(t!("home.joining"));
            }
            Join::Preview { preview, name } => {
                let address = preview.server.address.clone();
                if let Some(index) = home.servers.iter().position(|s| s.address == address) {
                    ui.label(ui::heading(t!("home.already_in")));
                    if ui::primary_large(ui, t!("home.open_it")).clicked() {
                        home.view = View::Server(index);
                        home.join = Join::Idle(None);
                    }
                    return;
                }
                let member = preview.member;
                match (&preview.name, member) {
                    (Some(server), false) => {
                        ui.label(ui::heading(t!("home.join_named", server = server)))
                    }
                    (None, false) => ui.label(ui::heading(t!("home.join_this"))),
                    (Some(server), true) => {
                        ui.label(ui::heading(t!("home.sign_in_named", server = server)))
                    }
                    (None, true) => ui.label(ui::heading(t!("home.sign_in_this"))),
                };
                ui.weak(host(&address));
                ui.add_space(12.0);
                // A member keeps the name and picture they have there.
                let field = if member {
                    ui.label(t!("home.already_member"));
                    None
                } else {
                    let label = ui.label(t!("home.your_name_here"));
                    let field = ui
                        .add(
                            ui::field(name)
                                .hint_text(t!("home.name_hint"))
                                .desired_width(300.0),
                        )
                        .labelled_by(label.id);
                    Some(field)
                };
                ui.add_space(8.0);
                if preview.invite.is_none() && !preview.open && !member {
                    ui.label(t!("home.needs_invite"));
                    if ui::secondary_large(ui, t!("common.back")).clicked() {
                        home.join = Join::Idle(None);
                    }
                    return;
                }
                if member {
                    // Said above already.
                } else if preview.invite.is_none() {
                    ui.label(t!("home.open_server"));
                } else if !preview.known {
                    ui.label(t!("home.first_time"));
                }
                CollapsingHeader::new(t!("common.details")).show(ui, |ui| {
                    ui.label(t!("home.detail_address", address = address));
                    ui.label(format!(
                        "{}  {}",
                        t!("home.server_id"),
                        fingerprint(&preview.server.server_id)
                    ));
                    if let Some(tls) = &preview.server.tls_fingerprint {
                        ui.label(t!(
                            "home.detail_certificate",
                            fingerprint = fingerprint(tls)
                        ));
                    }
                    ui.weak(t!("home.pins_help"));
                });
                ui.add_space(8.0);
                let button = if member {
                    t!("home.sign_in")
                } else {
                    t!("common.join")
                };
                let submit = field.is_some_and(|field| submitted(ui, &field));
                if ui::primary_large(ui, button).clicked() || submit {
                    let name = name.trim();
                    let admission = match (member, preview.invite) {
                        (true, _) => Admission::Member,
                        (false, Some(code)) => Admission::Invite(code),
                        (false, None) => Admission::Open,
                    };
                    let commands = home.net.start(
                        preview.server.clone(),
                        Some(admission),
                        (!member && !name.is_empty()).then(|| name.to_owned()),
                    );
                    let label = preview.name.as_deref().unwrap_or(t!("home.new_server"));
                    home.servers
                        .push(new_server(preview.server.clone(), label, commands));
                    if !member {
                        let mut settings = home.voice.audio.settings();
                        settings.hide_offline.joined(&address);
                        home.voice.audio.set_settings(settings);
                        let name = name.to_owned();
                        home.change_profile(|settings, _| {
                            settings.joined(&address, &name);
                            Vec::new()
                        });
                    }
                    home.left.remove(&address);
                    home.join = Join::Joining(address);
                } else if ui::secondary_large(ui, t!("common.cancel")).clicked() {
                    home.join = Join::Idle(None);
                }
            }
        }
    });
}

/// The official servers not joined or dismissed yet, each with Join and Dismiss.
fn official_servers(ui: &mut Ui, home: &mut Home) {
    let audio = home.voice.audio.clone();
    let mut settings = audio.settings();
    let joined: Vec<_> = home.servers.iter().map(|server| &server.address).collect();
    let shown = official::shown(&joined, &settings.dismissed_official);
    let mut pick = None;
    let mut changed = false;
    if !shown.is_empty() {
        ui.add_space(ui::SPACE_XL);
        ui.label(ui::title(t!("home.official_servers")));
    }
    for listed in shown {
        ui::card(ui, |ui| {
            ui::trailing_row(
                ui,
                |ui| {
                    let join = ui::primary(ui, t!("common.join"));
                    join.widget_info(|| {
                        WidgetInfo::labeled(
                            Role::Button,
                            true,
                            t!("home.join_named_button", server = listed.name),
                        )
                    });
                    let dismiss = ui::secondary(ui, t!("common.dismiss"));
                    dismiss.widget_info(|| {
                        WidgetInfo::labeled(
                            Role::Button,
                            true,
                            t!("home.dismiss_named", server = listed.name),
                        )
                    });
                    if dismiss.clicked() {
                        settings.dismissed_official.push(official::id(&listed));
                        changed = true;
                    } else if join.clicked() {
                        pick = Some(listed.clone());
                    }
                },
                |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add(Label::new(ui::bold(&listed.name)).wrap());
                    ui.add(Label::new(ui::caption(host(&listed.server.address))).truncate());
                },
            );
        });
    }
    if !settings.dismissed_official.is_empty() && ui.link(t!("home.show_official_again")).clicked()
    {
        settings.dismissed_official.clear();
        changed = true;
    }
    if changed {
        audio.set_settings(settings);
        home.publish_settings();
    }
    if let Some(listed) = pick {
        home.join = Join::Checking;
        let probe = home.index(&listed.server.address).is_none();
        home.net.check_listed(listed, probe);
    }
}

/// Settings → Privacy: everyone this person blocked, each with Unblock.
fn blocked_people(ui: &mut Ui, home: &mut Home) {
    ui::page_section(ui, t!("friends.blocked_section"));
    ui.label(ui::caption(t!("friends.blocked_help")));
    ui.add_space(ui::SPACE_XS);
    let blocked: Vec<(Id, String)> = (home.friends.contacts())
        .filter(|(_, contact)| contact.relation == Relation::Blocked)
        .map(|(person, contact)| {
            let name = match home.dm_server(person) {
                None if !contact.name.is_empty() => contact.name.clone(),
                _ => home.peer_name(person),
            };
            (*person, name)
        })
        .collect();
    if blocked.is_empty() {
        ui.label(ui::caption(t!("friends.empty_blocked")));
        return;
    }
    let mut unblock = None;
    for (person, name) in &blocked {
        ui::trailing_row(
            ui,
            |ui| {
                let button = ui::secondary(ui, t!("friends.unblock")).on_hover_text(name);
                if button.clicked() {
                    unblock = Some(*person);
                }
            },
            |ui| ui.add(Label::new(ui::bold(name)).truncate()),
        );
    }
    if let Some(person) = unblock {
        home.unblock(person, ui.input(|input| input.time));
    }
}

pub fn privacy_tab(ui: &mut Ui, home: &mut Home, keystore: &mut crate::keystore::Keystore) {
    let audio = home.voice.audio.clone();
    let mut settings = audio.settings();
    ui::page_section(ui, t!("home.direct_messages"));
    ui::caption_label(ui, t!("home.allow_dms_from"));
    ui.add_space(ui::SPACE_XS);
    let changed = ui::segmented(
        ui,
        &mut settings.friends_only,
        &[
            (
                false,
                t!("home.allow_dms_everyone"),
                t!("home.allow_dms_everyone_help"),
            ),
            (
                true,
                t!("home.allow_dms_friends"),
                t!("home.allow_dms_friends_help"),
            ),
        ],
    );
    if changed {
        audio.set_settings(settings.clone());
        home.publish_settings();
    }
    blocked_people(ui, home);
    ui::page_section(ui, t!("home.member_lists"));
    let mut hide = settings.hide_offline.on;
    let toggle = ui.add_enabled_ui(!home.servers.is_empty(), |ui| {
        ui::toggle_row(
            ui,
            &mut hide,
            t!("home.hide_offline"),
            t!("home.hide_offline_help"),
        )
    });
    if toggle.inner.changed() {
        let servers = home.servers.iter().map(|server| &server.address);
        settings.hide_offline.choose(hide, servers);
        for server in &home.servers {
            server.send(Command::SetHideOffline(hide));
        }
    }
    ui::page_section(ui, t!("previews.section"));
    for provider in previews::PROVIDERS {
        let mut on = settings.link_previews.get(provider.id()) == Some(&true);
        let help = t!("previews.toggle_help", site = provider.name());
        if ui::toggle_row(ui, &mut on, provider.name(), &help).changed() {
            settings.link_previews.insert(provider.id().into(), on);
            home.previews.allow(provider.id(), on);
        }
    }
    let mut create = settings.create_link_previews == Some(true);
    if ui::toggle_row(
        ui,
        &mut create,
        t!("previews.create"),
        t!("previews.create_help"),
    )
    .changed()
    {
        settings.create_link_previews = Some(create);
    }
    ui::page_section(ui, t!("phone.privacy_section"));
    ui::toggle_row(
        ui,
        &mut settings.phone_always_share,
        t!("phone.always_share"),
        t!("phone.always_share_help"),
    );
    ui::page_section(ui, t!("home.server_directory"));
    ui::banner(ui, "privacy-directory", t!("home.directory_help"));
    ui::setting_row(
        ui,
        t!("home.directory_address"),
        t!("home.directory_address_help"),
        |ui| {
            ui.add(
                ui::field(&mut settings.index_url)
                    .hint_text(official::DIRECTORY)
                    .desired_width(FIELD_WIDTH),
            )
            .widget_info(|| {
                WidgetInfo::labeled(Role::TextInput, true, t!("home.directory_address"))
            });
        },
    );
    ui::setting_row(
        ui,
        t!("home.certificate_fingerprint"),
        t!("home.certificate_fingerprint_help"),
        |ui| {
            ui.add(ui::field(&mut settings.index_fingerprint).desired_width(FIELD_WIDTH))
                .widget_info(|| {
                    WidgetInfo::labeled(Role::TextInput, true, t!("home.certificate_fingerprint"))
                });
        },
    );
    if !settings.index_url.trim().is_empty()
        && let Err(problem) = directory(&settings)
    {
        error_text(ui, Some(&problem));
    }
    audio.set_settings(settings);
    crate::key_storage::settings(ui, &mut home.key_storage, keystore, &home.identity);
}

/// The configured directory and its optional certificate pin, or the official one.
fn directory(settings: &crate::audio::Settings) -> Result<(url::Url, Option<Fingerprint>), String> {
    let url = settings.index_url.trim();
    if url.is_empty() {
        return Ok((
            https_url(official::DIRECTORY).expect("official directory"),
            None,
        ));
    }
    let url = https_url(url).ok_or(t!("home.use_https"))?;
    let pin = settings.index_fingerprint.trim();
    let pin = (!pin.is_empty())
        .then(|| decode(pin).map_err(|_| t!("home.bad_fingerprint")))
        .transpose()?;
    Ok((url, pin))
}

fn find(ui: &mut Ui, home: &mut Home) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui::SPACE_XL);
        ui::max_width(ui, 640.0);
        ui.label(ui::heading(t!("home.find_servers")));
        if ui.link(t!("home.join_with_invite")).clicked() {
            home.view = View::Join;
        }
        let settings = home.voice.audio.snapshot();
        let find = &mut home.find;
        let mut page = find.auto_page();
        ui.horizontal(|ui| {
            let field = ui.add(
                ui::field(&mut find.query)
                    .hint_text(t!("home.search_hint"))
                    .desired_width(ui.available_width() - 90.0),
            );
            field.widget_info(|| WidgetInfo::labeled(Role::TextInput, true, t!("common.search")));
            if ui::primary(ui, t!("common.search")).clicked() || submitted(ui, &field) {
                page = Some(1);
            }
        });
        error_text(ui, find.error.as_deref());
        if find.error.is_some()
            && !find.searching
            && ui::secondary(ui, t!("common.try_again")).clicked()
        {
            page = Some(1);
        }
        if find.searching || find.checking {
            ui.spinner();
        } else if find.results.is_empty() && find.page > 0 {
            ui.weak(t!("home.no_servers_found"));
        }
        let mut pick = None;
        ScrollArea::vertical().show(ui, |ui| {
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                for listed in &find.results {
                    ui::card(ui, |ui| {
                        ui::trailing_row(
                            ui,
                            |ui| {
                                let join = ui
                                    .add_enabled_ui(!find.checking, |ui| {
                                        ui::primary(ui, t!("common.join"))
                                    })
                                    .inner;
                                join.widget_info(|| {
                                    WidgetInfo::labeled(
                                        Role::Button,
                                        true,
                                        t!("home.join_named_button", server = listed.name),
                                    )
                                });
                                if join.clicked() {
                                    pick = Some(listed.clone());
                                }
                            },
                            |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.add(Label::new(ui::title(&listed.name)).wrap());
                                    if listed.open {
                                        let fill = ui::accent(ui).base;
                                        ui::badge(ui, t!("home.open_joining"), fill);
                                    }
                                });
                            },
                        );
                        if !listed.description.is_empty() {
                            ui.add(Label::new(&listed.description).wrap());
                        }
                        ui.label(ui::caption(t!("home.members", count = listed.members)));
                    });
                }
                if find.more
                    && !find.searching
                    && ui::secondary(ui, t!("common.show_more")).clicked()
                {
                    page = Some(find.page + 1);
                }
            });
        });
        if let Some(listed) = pick {
            find.checking = true;
            find.error = None;
            let joined = home.servers.iter();
            let probe = !joined
                .map(|s| &s.address)
                .any(|a| *a == listed.server.address);
            home.net.check_listed(listed, probe);
        }
        if let Some(page) = page {
            match directory(&settings) {
                Ok((url, pin)) => {
                    find.searching = true;
                    find.error = None;
                    if page == 1 {
                        find.results.clear();
                    }
                    home.net
                        .search(url, pin, find.query.trim().to_owned(), page);
                }
                Err(problem) => find.error = Some(problem),
            }
        }
    });
}

pub(crate) fn new_server(
    known: KnownServer,
    label: &str,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
) -> Server {
    Server {
        address: known.address.clone(),
        known,
        label: label.into(),
        status: Status::Connecting,
        compatibility: Compatibility::default(),
        max_files: rusttalk_core::messages::LEGACY_MAX_ATTACHMENTS,
        order_unavailable: false,
        dir: Directory::default(),
        chats: HashMap::new(),
        open: None,
        events_open: false,
        events: events::View::default(),
        voice: HashMap::new(),
        names: HashMap::new(),
        commands,
        view_as: None,
    }
}

/// A server address typed on the join page: `https://host[:port]`, or just `host[:port]`.
fn server_address(text: &str) -> Option<url::Url> {
    let text = text.strip_suffix('/').unwrap_or(text);
    let url = match text.starts_with("https://") {
        true => https_url(text),
        false if text.contains("://") => None,
        false => https_url(&format!("https://{text}")),
    }?;
    (url.path() == "/").then_some(url)
}

fn error_slot(join: &mut Join) -> &mut Option<String> {
    if !matches!(join, Join::Idle(_)) {
        *join = Join::Idle(None);
    }
    match join {
        Join::Idle(error) => error,
        _ => unreachable!("set to idle above"),
    }
}

/// A member change as a sentence; `action` is the ID `net::directory` gives it.
fn notice(name: &str, action: &str) -> String {
    match action {
        "joined" => t!("notice.joined", name = name),
        "left" => t!("notice.left", name = name),
        other => format!("{name} {other}"),
    }
}

fn time(at: i64) -> String {
    let Some(at) = DateTime::from_timestamp(at, 0) else {
        return String::new();
    };
    let at = at.with_timezone(&Local);
    if at.date_naive() == Local::now().date_naive() {
        crate::i18n::time(at, "time.clock")
    } else {
        crate::i18n::time(at, "time.day_clock")
    }
}

/// A voice channel shares its chat pane with the stage only while someone is in voice.
fn voice_pane(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    let server = &home.servers[index];
    let joined = home.voice.in_call(server, &channel);
    let occupied = joined
        || server
            .voice
            .get(&channel)
            .is_some_and(|peers| !peers.is_empty());
    let title = server
        .chat_title(&channel)
        .map(|(name, _)| name)
        .unwrap_or_default();
    let mut join = false;
    let mut split = home.layout.call_split();
    pane_title_with(
        ui,
        "room-title",
        &format!("🔊 {title}"),
        Some(channel),
        |ui| {
            layout::members_button(ui, &mut home.layout);
            ui.add_space(ui::SPACE_S);
            if occupied {
                layout::split_buttons(ui, &mut split);
            } else {
                join = ui
                    .add_enabled_ui(server.status == Status::Online, |ui| {
                        ui::primary(ui, t!("call.join_voice")).clicked()
                    })
                    .inner;
            }
        },
    );
    home.layout.set_call_split(split);
    focus::banner(ui, home.servers[index].channel(&channel));
    if join {
        call::join(home, index, channel);
    }
    // When chat is visible, show feedback beside its composer or shared files.
    if occupied && split.chat_hidden {
        toast_line(ui, &mut home.toast);
    }
    ui.add_space(ui::SPACE_S);
    let mut area = ui.available_rect_before_wrap();
    // Phones hide the sidebar and its voice panel here, so the call's controls come
    // along at the bottom.
    if joined && home.layout.phone {
        let top = area.bottom() - ui::TOUCH_TARGET;
        let bar = Rect::from_min_max(egui::pos2(area.left(), top), area.max);
        area.max.y = top - ui::SPACE_S;
        let mut ui = ui.new_child(UiBuilder::new().max_rect(bar));
        call::phone_controls(&mut ui, home);
    }
    let (stage, divider, chat) = if occupied {
        layout::split(area, &split)
    } else {
        (None, None, Some(area))
    };
    if let Some(rect) = stage {
        let mut ui = ui.new_child(UiBuilder::new().max_rect(rect));
        call::stage(&mut ui, home, index, channel);
    }
    if let Some(rect) = chat {
        let mut ui = ui.new_child(UiBuilder::new().max_rect(rect));
        chat_pane(&mut ui, home, index, channel);
    }
    home.layout.split_dragging = divider
        .is_some_and(|divider| layout::split_divider(ui, divider, area, &mut home.layout.split));
    ui.allocate_rect(area, Sense::hover());
}

/// Where a queued message goes: a channel by its server's address, or a DM.
#[derive(Clone, PartialEq)]
enum To {
    Channel(url::Url, Id),
    Dm(Id),
}

struct Outgoing {
    to: To,
    draft: Draft,
    /// The preview being made; `None` once removed, or for a message only waiting its turn.
    preview: Option<Making>,
}

enum Making {
    /// Waiting for the person to say whether the app may visit the link.
    Ask(url::Url),
    /// Being made; dropping it stops the fetch.
    Job(link_cards::Job),
}

/// A new message with a link to some other site waits for its preview; any message
/// sent meanwhile waits behind it. Returns the draft when it can go right away.
fn queue_preview(
    ui: &Ui,
    voice: &Voice,
    sending: &mut VecDeque<Outgoing>,
    to: &To,
    draft: Draft,
) -> Option<Draft> {
    // A page saved again keeps its preview while its link stays the same.
    let link = (draft.edit_of.is_none()
        && draft.sticker.is_none()
        && draft.poll.is_none()
        && !draft.keep.iter().any(files::is_card))
    .then(|| link_cards::candidate(markdown::cached(ui, &draft.text).links()))
    .flatten();
    // Asked the first time (and each time until Always or Never).
    let preview = match (link, voice.audio.settings().create_link_previews) {
        (Some(url), Some(true)) => Some(Making::Job(link_cards::Job::start(url, ui.ctx()))),
        (Some(url), None) => Some(Making::Ask(url)),
        _ => None,
    };
    if preview.is_none() && !sending.iter().any(|o| o.to == *to) {
        return Some(draft);
    }
    sending.push_back(Outgoing {
        to: to.clone(),
        draft,
        preview,
    });
    None
}

/// Above the composer: the question whether to make a preview, or that one is being made.
fn preview_status(ui: &mut Ui, voice: &Voice, out: &mut Outgoing) {
    match &out.preview {
        Some(Making::Ask(url)) => {
            let url = url.clone();
            let host = url.host_str().unwrap_or_default().to_owned();
            ui.weak(t!("previews.ask_create", host = host));
            ui.horizontal(|ui| {
                let mut answer = None;
                if ui::primary(ui, t!("previews.always")).clicked() {
                    answer = Some(Some(true));
                }
                if ui::secondary(ui, t!("previews.not_now")).clicked() {
                    answer = Some(None);
                }
                if ui::secondary(ui, t!("previews.never")).clicked() {
                    answer = Some(Some(false));
                }
                let Some(choice) = answer else {
                    return;
                };
                if choice.is_some() {
                    let mut settings = voice.audio.settings();
                    settings.create_link_previews = choice;
                    voice.audio.set_settings(settings);
                }
                out.preview = (choice == Some(true))
                    .then(|| Making::Job(link_cards::Job::start(url, ui.ctx())));
            });
        }
        Some(Making::Job(_)) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(t!("previews.adding"));
                if ui::small_close(ui, t!("previews.remove")).clicked() {
                    out.preview = None;
                }
            });
        }
        None => {}
    }
}

/// Ask for the page before the oldest loaded message, as scrolling up the chat does.
/// A DM asks each copy on its own, so one slow server doesn't hold up the others.
fn load_older(servers: &[Server], threads: &mut HashMap<Id, Thread>, at: At, chat: &mut Chat) {
    match at {
        At::Channel(index, channel) => {
            if let Some(oldest) = chat.oldest {
                chat.loading = true;
                servers[index].send(Command::Older(channel, oldest));
            }
        }
        At::Dm(peer) => {
            let online = |url: &url::Url| {
                (servers.iter()).any(|s| s.address == *url && s.status == Status::Online)
            };
            let thread = threads.entry(peer).or_default();
            for (url, channel, before) in thread.older(online) {
                if let Some(server) = servers.iter().find(|s| s.address == url) {
                    chat.loading = true;
                    server.send(match before {
                        Some(before) => Command::Older(channel, before),
                        // Its cursor was gone from that server: start over there.
                        None => Command::Open(channel),
                    });
                }
            }
        }
    }
}

/// Which chat a pane shows.
#[derive(Clone, Copy)]
pub(crate) enum At {
    /// A text or voice channel: server index and channel.
    Channel(usize, Id),
    /// A DM, by the other person, over every server holding a copy.
    Dm(Id),
}

fn chat_pane(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    if home.servers[index].chat_title(&channel).is_none() {
        home.servers[index].open = None;
        return;
    }
    let server = &mut home.servers[index];
    let mut chat = server.chats.remove(&channel).unwrap_or_default();
    // A membership notice can arrive before anyone has opened this channel.
    if !chat.opened && server.status == Status::Online {
        chat.opened = true;
        server.send(Command::Open(channel));
        chat.loading = true;
    }
    let kind = home.servers[index].channel(&channel).map(ChannelKind::of);
    if files_shown(ui.ctx(), channel) {
        files_view(ui, home, index, channel, &mut chat);
    } else if gallery::shown(ui.ctx(), channel) {
        chat.shared_asked = false;
        channel_title(ui, home, index, channel);
        toast_line(ui, &mut home.toast);
        // Pages and forums have no chat to scroll to a message in.
        let jumps = !matches!(kind, Some(ChannelKind::Page | ChannelKind::Forum));
        gallery::view(ui, home, At::Channel(index, channel), &mut chat, jumps);
    } else {
        // Reopening the files loads them again, with what others added meanwhile.
        chat.shared_asked = false;
        match kind {
            Some(ChannelKind::Page) => page_view(ui, home, index, channel, &mut chat),
            Some(ChannelKind::Forum) => forum::view(ui, home, index, channel, &mut chat),
            _ => {
                chat_view(ui, home, At::Channel(index, channel), &mut chat);
            }
        }
    }
    home.servers[index].chats.insert(channel, chat);
}

fn files_id(channel: Id) -> UiId {
    UiId::unique(("files-shown", channel))
}

/// Whether a channel shows its shared files instead of its chat or page.
fn files_shown(ctx: &egui::Context, channel: Id) -> bool {
    ctx.data(|d| d.get_temp(files_id(channel))).unwrap_or(false)
}

/// Switch a channel between its chat (or page) and its shared files.
fn files_button(ui: &mut Ui, channel: Id) {
    let shown = files_shown(ui.ctx(), channel);
    let tip = if shown {
        t!("files.shared_hide")
    } else {
        t!("files.shared_show")
    };
    // A soft fill while the files show, like the members toggle.
    let look = if shown {
        ui::Look::Plain
    } else {
        ui::Look::Ghost
    };
    if ui::icon_button(ui, "🗀", tip, look, Vec2::new(30.0, 26.0)).clicked() {
        if !shown {
            gallery::set_shown(ui.ctx(), channel, false);
        }
        ui.ctx()
            .data_mut(|d| d.insert_temp(files_id(channel), !shown));
    }
}

/// A channel pane's title with its buttons, for the views in place of its chat.
fn channel_title(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    let server = &home.servers[index];
    // A voice channel's call above already shows the title.
    if !server.is_voice(&channel) {
        let title = server
            .channel(&channel)
            .map_or_else(String::new, |c| format!("{} {}", channel_icon(c), c.name));
        let in_server = matches!(home.view, View::Server(_));
        let layout = in_server.then_some(&mut home.layout);
        pane_title(ui, "chat-title", &title, layout, Some(channel));
    }
}

/// A channel's shared files, like a small drive: everyone who can read the channel
/// downloads them, people who can write upload, and the uploader or a moderator removes.
fn files_view(ui: &mut Ui, home: &mut Home, index: usize, channel: Id, chat: &mut Chat) {
    channel_title(ui, home, index, channel);
    toast_line(ui, &mut home.toast);
    let server = &home.servers[index];
    let online = server.status == Status::Online;
    let can_upload =
        online && server.can_write(&channel) && !server.dir.perms.preview && files::PICKER;
    if !chat.shared_asked {
        chat.shared_asked = true;
        server.send(Command::SharedFiles(channel));
    }
    let mut upload = false;
    let mut remove = None;
    ui.add_space(ui::SPACE_S);
    ui.horizontal(|ui| {
        ui.label(ui::heading(t!("files.shared_title")));
        if can_upload {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                upload = ui::primary(ui, t!("files.upload")).clicked();
            });
        }
    });
    if let Some((what, done)) = &chat.uploading {
        ui.horizontal(|ui| {
            ui.weak(t!("home.uploading", file = what));
            ui.add(
                egui::ProgressBar::new(*done)
                    .show_percentage()
                    .desired_width(200.0),
            );
        });
    }
    ui.add_space(ui::SPACE_S);
    ScrollArea::vertical()
        .id_salt(("shared-files", channel))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            match &chat.shared {
                None => {
                    ui.vertical_centered(|ui| ui.spinner());
                }
                Some(list) if list.is_empty() => {
                    let hint = if can_upload {
                        t!("files.shared_empty_upload")
                    } else {
                        t!("files.shared_empty")
                    };
                    empty(ui, t!("files.shared_title"), hint);
                }
                Some(list) => {
                    for shared in list {
                        let note = t!(
                            "files.shared_by",
                            name = server.member_name(&shared.person),
                            time = time(shared.at)
                        );
                        let removable = online
                            && (shared.person == server.dir.me || server.dir.perms.delete_messages);
                        let address = &server.address;
                        let file = &shared.file;
                        if files::shared_card(ui, &mut home.files, address, file, &note, removable)
                        {
                            remove = Some(shared.clone());
                        }
                        ui.add_space(ui::SPACE_S);
                    }
                }
            }
        });
    if upload {
        let ask = Ask::Share(server.address.clone(), channel);
        home.files.ask(ui.ctx(), ask);
    }
    if let Some(shared) = remove {
        home.dialog = Some(Dialog::Unshare(index, channel, Box::new(shared)));
    }
}

/// A page channel: its newest message, shown as one Markdown page with its pictures
/// and link previews. Saving an edit posts the whole page again as a new message, so
/// it stays end-to-end encrypted; its pictures go along by reference.
fn page_view(ui: &mut Ui, home: &mut Home, index: usize, channel: Id, chat: &mut Chat) {
    let in_server = matches!(home.view, View::Server(_));
    let server = &home.servers[index];
    let title = server
        .channel(&channel)
        .map_or_else(String::new, |c| format!("{} {}", channel_icon(c), c.name));
    let can_edit = server.can_write(&channel) && !server.dir.perms.preview;
    let online = server.status == Status::Online;
    pane_title(
        ui,
        "chat-title",
        &title,
        in_server.then_some(&mut home.layout),
        Some(channel),
    );
    toast_line(ui, &mut home.toast);
    let Home {
        servers,
        files,
        voice,
        previews,
        sending,
        toast,
        ..
    } = home;
    let server = &servers[index];
    let dir = &server.dir;
    let to = To::Channel(server.address.clone(), channel);
    let now = ui.input(|input| input.time);
    // Membership notices are not part of the page.
    // A copy, so the editor below can change the chat's staged pictures.
    let page = (chat.messages.iter().rev())
        .find(|msg| matches!(msg.body, Body::Text(_) | Body::Unreadable) && msg.sticker.is_none())
        .cloned();
    let page = page.as_ref();
    let editing_id = UiId::unique(("page-editing", channel));
    let mut editing = can_edit && ui.data(|d| d.get_temp(editing_id)).unwrap_or(false);
    // Pictures the editor took off; the rest stay with the page when it is saved.
    let removed_id = UiId::unique(("page-removed", channel));
    let mut removed: Vec<Id> = ui.data(|d| d.get_temp(removed_id)).unwrap_or_default();
    // Pictures only: the sender-made link preview is kept on save only if its link is.
    let kept: Vec<Attachment> = page
        .map(|msg| msg.files.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|file| !files::is_card(file) && !removed.contains(&file.file.id))
        .cloned()
        .collect();
    let max = server.max_files;
    let room = max.saturating_sub(kept.len());
    if chat.staged.len() > room {
        for file in chat.staged.drain(room..) {
            files::discard(&file.path);
        }
        *toast = Some((t!("files.too_many", max = max), now + 6.0));
    }
    let mut save = false;
    let mut cancel = false;
    ui.add_space(ui::SPACE_S);
    if can_edit {
        ui.horizontal(|ui| {
            if editing {
                let ready = online
                    && !(chat.draft.trim().is_empty() && kept.is_empty() && chat.staged.is_empty());
                save = ui
                    .add_enabled_ui(ready, |ui| ui::primary(ui, t!("common.save")))
                    .inner
                    .clicked();
                if ui::secondary(ui, t!("common.cancel")).clicked() {
                    cancel = true;
                    editing = false;
                }
                let add = format!("🖼 {}", t!("home.page_add_pictures"));
                if ui
                    .add_enabled_ui(online && chat.staged.len() < room && files::PICKER, |ui| {
                        ui::secondary(ui, add.as_str())
                    })
                    .inner
                    .clicked()
                {
                    files.ask(ui.ctx(), Ask::PagePictures(server.address.clone(), channel));
                }
                let count = chat.draft.chars().count();
                ui.weak(format!("{count} / {MAX_TEXT_CHARS}"));
            } else if ui::secondary(ui, format!("✏ {}", t!("home.edit_page")).as_str()).clicked()
            {
                // A draft kept from a failed save wins over the page as it is.
                if chat.draft.is_empty() && chat.staged.is_empty() {
                    removed.clear();
                    if let Some(Msg {
                        body: Body::Text(text),
                        ..
                    }) = page
                    {
                        chat.draft = text.clone();
                    }
                }
                editing = true;
            }
        });
        ui.add_space(ui::SPACE_S);
    }
    if let Some(out) = (sending.iter_mut()).find(|o| o.to == to && o.preview.is_some()) {
        preview_status(ui, voice, out);
    }
    let settings = voice.audio.snapshot();
    let embeds = Embeds {
        servers,
        previews,
        allowed: &settings.link_previews,
    };
    let holder = (&server.address, &server.commands);
    let mut pick = None;
    ScrollArea::vertical()
        .id_salt(("page", channel))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if editing {
                ui.add(
                    ui::text_area(&mut chat.draft)
                        .char_limit(MAX_TEXT_CHARS)
                        .desired_rows(20)
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(ui::SPACE_S);
                ui.horizontal_wrapped(|ui| {
                    for file in &kept {
                        ui.vertical(|ui| {
                            files::attachments(ui, files, holder, std::slice::from_ref(file), &[]);
                            if ui::small_close(ui, t!("common.remove")).clicked() {
                                removed.push(file.file.id);
                            }
                        });
                    }
                });
                files::staged(ui, chat);
                return;
            }
            match page {
                Some(
                    msg @ Msg {
                        body: Body::Text(text),
                        ..
                    },
                ) => {
                    let mention = mentions::Message::cached(ui, msg, dir, channel);
                    mention.doc.show(ui, dir, &mention, false, false);
                    // A sender-made preview must be about one of the page's own links.
                    let links: Vec<String> = markdown::cached(ui, text)
                        .links()
                        .map(String::from)
                        .collect();
                    files::attachments(ui, files, holder, &msg.files, &links);
                    for (provider, key) in previews::supported(mention.doc.links()) {
                        if let Some(chosen) = link_preview(ui, &embeds, provider, &key) {
                            pick = Some(chosen);
                        }
                    }
                    ui.add_space(ui::SPACE_S);
                    let edited = t!(
                        "home.page_updated",
                        name = server.member_name(&msg.person),
                        time = time(msg.at)
                    );
                    ui.label(ui::caption(&edited));
                }
                Some(_) => {
                    ui.weak(t!("home.page_unreadable"));
                }
                None if chat.loading => {
                    ui.vertical_centered(|ui| ui.spinner());
                }
                None if can_edit => empty(ui, &title, t!("home.page_empty_editor")),
                None => empty(ui, &title, t!("home.page_empty")),
            }
        });
    if let Some(Pick::Previews(id, allow)) = pick {
        let mut settings = voice.audio.settings();
        settings.link_previews.insert(id.into(), allow);
        voice.audio.set_settings(settings);
        previews.allow(id, allow);
    }
    if cancel {
        // Else the next Edit reopens the cancelled text, not the current page.
        chat.clear_draft();
        for file in chat.staged.drain(..) {
            files::discard(&file.path);
        }
        removed.clear();
    }
    if save {
        match rusttalk_core::keys::random_key() {
            Ok(id) => {
                // Keep leading indentation: a page may start with a code block.
                let text = chat.draft.trim_end().to_owned();
                let mut keep = kept;
                // The link didn't change: keep its preview rather than visit the site again.
                let link = link_cards::candidate(markdown::cached(ui, &text).links());
                let card = (page.into_iter().flat_map(|msg| &msg.files)).find(|file| {
                    files::is_card(file) && link.is_some() && files.card_url(&file.file.id) == link
                });
                if keep.len() + chat.staged.len() < max {
                    keep.extend(card.cloned());
                }
                let draft = Draft {
                    forum_post: false,
                    queued: Default::default(),
                    id: *id,
                    channel,
                    peer: None,
                    text,
                    reply_to: None,
                    edit_of: None,
                    revision: 0,
                    files: chat.staged.drain(..).map(|file| file.path).collect(),
                    keep,
                    sticker: None,
                    friend: None,
                    poke: false,
                    preview: None,
                    inline: Vec::new(),
                    poll: None,
                    voice_request: None,
                };
                if let Some(draft) = queue_preview(ui, voice, sending, &to, draft) {
                    server.send(Command::Send(draft));
                }
                chat.clear_draft();
                editing = false;
            }
            Err(_) => tracing::warn!("no randomness for a page ID"),
        }
    }
    ui.data_mut(|d| {
        d.insert_temp(editing_id, editing);
        d.insert_temp(removed_id, removed);
    });
}

fn dm_pane(ui: &mut Ui, home: &mut Home, peer: Id) {
    if home.friends.relation(&peer) == Relation::Blocked {
        let name = home.peer_name(&peer);
        empty(ui, &name, t!("friends.blocked_dm"));
        ui.vertical_centered(|ui| {
            if ui::secondary(ui, t!("friends.unblock")).clicked() {
                home.befriend(peer, Act::Unblock, ui.input(|input| input.time));
            }
        });
        return;
    }
    // Load every copy, also ones made after the DM was opened.
    for (index, channel) in home.replicas(&peer) {
        let server = &home.servers[index];
        let thread = home.threads.entry(peer).or_default();
        if server.status == Status::Online && thread.open(&server.address, channel) {
            server.send(Command::Open(channel));
        }
    }
    if let Some((index, pick)) = dm_title(ui, home, peer) {
        person::apply(home, index, peer, pick, ui.input(|input| input.time));
    }
    let mut chat = std::mem::take(&mut home.threads.entry(peer).or_default().chat);
    let act = if gallery::shown(ui.ctx(), peer) {
        toast_line(ui, &mut home.toast);
        gallery::view(ui, home, At::Dm(peer), &mut chat, true);
        None
    } else {
        chat_view(ui, home, At::Dm(peer), &mut chat)
    };
    let thread = home.threads.entry(peer).or_default();
    thread.chat = chat;
    thread.refresh();
    if let Some(act) = act {
        home.befriend(peer, act, ui.input(|input| input.time));
    }
}

fn handle_live_file(
    ui: &Ui,
    voice: &mut Voice,
    files: &mut Files,
    chat: &Chat,
    server: Option<&Server>,
    action: LiveAction,
) {
    match action {
        LiveAction::Save(id, name) => {
            if !voice.live_files.iter().any(|file| file.id == id)
                && let Some(msg) = chat
                    .messages
                    .iter()
                    .find(|m| m.live.as_ref().is_some_and(|l| l.0 == id))
            {
                voice.live_files.push(LiveFile {
                    id,
                    chat: true,
                    from: Some(msg.person),
                    name: name.clone(),
                    size: msg.live.as_ref().map_or(0, |l| l.2),
                    state: LiveState::Offered,
                    receivers: Vec::new(),
                });
            }
            if let Some(server) = server {
                let address = server.address.clone();
                files.ask(ui.ctx(), Ask::Accept(address, id, name));
            }
        }
        LiveAction::Remove(id, stop) => {
            if stop && let Some(server) = server {
                server.send(Command::CancelFile(id));
            }
            match voice.live_files.iter_mut().find(|file| file.id == id) {
                Some(file) => file.state = LiveState::Ended,
                None => voice.live_files.push(LiveFile {
                    id,
                    chat: true,
                    from: None,
                    name: String::new(),
                    size: 0,
                    state: LiveState::Ended,
                    receivers: Vec::new(),
                }),
            }
        }
    }
}

/// Shows a chat. In a DM, also returns what the person picked on the friend card.
fn chat_view(ui: &mut Ui, home: &mut Home, at: At, chat: &mut Chat) -> Option<Act> {
    let (title, dm, in_voice, icon) = match at {
        At::Channel(index, channel) => {
            let server = &home.servers[index];
            let (title, dm) = server.chat_title(&channel).unwrap_or_default();
            let icon = server.channel(&channel).map_or("#", channel_icon);
            (title, dm, server.is_voice(&channel), icon)
        }
        At::Dm(peer) => (home.peer_name(&peer), true, false, "@"),
    };
    let name = title.clone();
    // A voice channel's chat sits under its call, which already shows the title.
    let title = if dm {
        format!("@ {title}")
    } else {
        format!("{icon} {title}")
    };
    // The server for names and pictures, the servers a message goes to, and the
    // channel for live files.
    let (named, targets, live_at, key) = match at {
        At::Channel(index, channel) => (Some(index), vec![index], Some((index, channel)), channel),
        At::Dm(peer) => {
            let online = |index: &usize| home.servers[*index].status == Status::Online;
            let replicas = home.replicas(&peer);
            let named = home.dm_server(&peer);
            // The copy on the server shown, so mentions treat it as a DM.
            let key = (replicas.iter())
                .find(|(index, _)| Some(*index) == named)
                .map_or(peer, |(_, channel)| *channel);
            let live_at = replicas.into_iter().find(|(index, _)| online(index));
            let targets = home.shared(&peer).into_iter().filter(online).collect();
            (named, targets, live_at, key)
        }
    };
    // Every shared server is gone: what was loaded stays readable.
    let read_only = matches!(at, At::Dm(peer) if home.shared(&peer).is_empty());
    let max_files = home.max_files(at);
    let (relation, request) = match at {
        At::Dm(peer) => (
            home.friends.relation(&peer),
            home.friends.is_request(&peer, home.friends_only()),
        ),
        At::Channel(..) => (Relation::None, false),
    };
    let in_server = matches!(home.view, View::Server(_));
    // Without a server for the DM, names come from what the servers remember.
    let me = *home.identity.certificate.person_id();
    let people = match at {
        At::Dm(peer) => vec![peer, me],
        At::Channel(..) => Vec::new(),
    };
    let fallback = Directory {
        me,
        members: (people.into_iter())
            .map(|person| Member {
                person,
                name: Some(home.peer_name(&person)),
                devices: Vec::new(),
                roles: Vec::new(),
                avatar: None,
                presence: Presence::Offline,
                hide_offline: false,
                bio: None,
                voice_muted: false,
            })
            .collect(),
        ..Directory::default()
    };
    let Home {
        servers,
        friends,
        threads,
        toast,
        dialog,
        files,
        emoji_picker,
        voice,
        layout,
        view,
        invite: invite_text,
        net,
        join: joining,
        previews,
        sending,
        ..
    } = home;
    let to = match at {
        At::Channel(index, channel) => To::Channel(servers[index].address.clone(), channel),
        At::Dm(peer) => To::Dm(peer),
    };
    let dir = named.map_or(&fallback, |index| &servers[index].dir);
    let online = !read_only && targets.iter().any(|i| servers[*i].status == Status::Online);
    let now = ui.input(|input| input.time);
    // A DM has its own title, with their profile (`dm_title`).
    if !in_voice && !dm {
        let files = match at {
            At::Channel(_, channel) => Some(channel),
            At::Dm(_) => None,
        };
        pane_title(ui, "chat-title", &title, in_server.then_some(layout), files);
    }
    let act = friends::dm_card(ui, relation, request, &name);
    let (hovered, dropped) = ui.input(|input| {
        let dropped: Vec<_> = (input.raw.dropped_files.iter())
            .map(|file| file.path().to_owned())
            .collect();
        (!input.raw.hovered_files.is_empty(), dropped)
    });
    if online
        && !dropped.is_empty()
        && let Some(problem) = files::stage(chat, dropped, max_files)
    {
        *toast = Some((problem, now + 6.0));
    }
    // Ctrl+V in this composer may attach a picture or copied files (see paste.rs).
    if online
        && ui.memory(|memory| memory.has_focus(composer_id(key)))
        && crate::paste::wanted(ui.ctx())
    {
        let ask = match at {
            At::Channel(index, channel) => Ask::Attach(servers[index].address.clone(), channel),
            At::Dm(peer) => Ask::AttachDm(peer),
        };
        files.paste(ui.ctx(), ask);
    }
    let attach = Panel::bottom("composer")
        // It floats over the chat: no line above, and the pane's margin on the
        // sides and below it, so its centre lines up with the user bar's.
        .frame(Frame::new().inner_margin(Margin {
            top: ui::SPACE_S as i8,
            ..Margin::ZERO
        }))
        .show_separator_line(false)
        .show(ui, |ui| {
            toast_line(ui, toast);
            if read_only {
                ui.weak(t!("home.dm_read_only", name = name));
                return None;
            }
            // Previewing: say whether those roles could post here, without a composer.
            if let At::Channel(index, channel) = at
                && servers[index].view_as.is_some()
            {
                let could = servers[index].can_write(&channel);
                ui.weak(if could {
                    t!("view_as.could_post")
                } else {
                    t!("view_as.read_only")
                });
                return None;
            }
            let hint = if dm {
                format!("@{name}")
            } else {
                format!("#{name}")
            };
            if let Some(out) = sending
                .iter_mut()
                .find(|o| o.to == to && o.preview.is_some())
            {
                preview_status(ui, voice, out);
            }
            // This server's emoji go by ID in a channel; every other server's picture goes
            // along as a small file (and in a DM, all of them do).
            let local = matches!(at, At::Channel(..)).then_some(dir);
            let here = match at {
                At::Channel(index, _) => Some(index),
                At::Dm(_) => None,
            };
            let scope = picker::Scope {
                local,
                others: (servers.iter().enumerate())
                    .filter(|(index, server)| Some(*index) != here && !server.dir.emoji.is_empty())
                    .map(|(_, server)| (server.name(), &server.dir))
                    .collect(),
            };
            let (command, attach) = composer(
                ui,
                chat,
                dir,
                online,
                key,
                &hint,
                (emoji_picker, &scope),
                (toast, now),
            );
            // A new message with a link to some other site waits for its preview; any
            // message sent meanwhile waits behind it. Both show at once, as sending.
            let command = match command {
                Some(Command::Send(draft)) => {
                    chat.outgoing(me, &draft);
                    queue_preview(ui, voice, sending, &to, draft).map(Command::Send)
                }
                other => other,
            };
            match (at, command) {
                (At::Dm(peer), Some(Command::Send(draft))) => {
                    send_dm(servers, &targets, chat, toast, now, (me, peer), draft);
                }
                (At::Channel(index, _), Some(command)) => servers[index].send(command),
                (At::Dm(_), Some(_)) | (_, None) => {}
            }
            attach
        })
        .inner;
    match (attach, at, live_at) {
        (Some(false), At::Channel(index, channel), _) => {
            files.ask(
                ui.ctx(),
                Ask::Attach(servers[index].address.clone(), channel),
            );
        }
        (Some(false), At::Dm(peer), _) => files.ask(ui.ctx(), Ask::AttachDm(peer)),
        (Some(true), _, Some((index, channel))) => {
            files.ask(ui.ctx(), Ask::Live(servers[index].address.clone(), channel));
        }
        (Some(true), _, None) => *toast = Some((t!("home.waiting_try_again").into(), now + 6.0)),
        (None, ..) => {}
    }
    if online && hovered {
        drop_zone(ui, &title, max_files);
    }
    // The server a message's files and live offer come from.
    let holder = |msg: &Msg| {
        let copy =
            (msg.replicas.first()).and_then(|url| servers.iter().position(|s| s.address == *url));
        copy.or(named)
    };
    // Only the rows near the view are laid out (#461): first which messages show and
    // whether each starts a group, then their heights as far as known.
    let mut shown: Vec<(usize, bool)> = Vec::new();
    let mut last: Option<&Msg> = None;
    // A blocked person's message is one line of its own until shown.
    let hidden = |revealed: &HashSet<(Id, Id)>, msg: &Msg| {
        msg.person != me
            && friends.is_blocked(&msg.person)
            && !matches!(msg.body, Body::Notice { .. })
            && !revealed.contains(&(msg.person, msg.id))
    };
    for (at, msg) in chat.messages.iter().enumerate() {
        if matches!(msg.body, Body::Friend(signal, _) if !friends::shown(signal))
            || matches!(
                msg.body,
                Body::Poke(_) | Body::PollPart(_) | Body::VoiceRequest(_)
            )
        {
            continue;
        }
        let start = !last.is_some_and(|prev| {
            prev.person == msg.person
                && prev.backfilled == msg.backfilled
                && !matches!(prev.body, Body::Notice { .. } | Body::Friend(..))
                && msg.at - prev.at < 300
                && msg.reply_to.is_none()
                && !hidden(&chat.revealed, prev)
                && !hidden(&chat.revealed, msg)
        });
        shown.push((at, start));
        last = Some(msg);
    }
    let mut after = last.map(|msg| (msg.person, matches!(msg.body, Body::Notice { .. })));
    let keys: Vec<rows::Key> = (shown.iter())
        .map(|&(at, start)| (chat.messages[at].person, chat.messages[at].id, start))
        .collect();
    let mut rows = std::mem::take(&mut chat.rows);
    // The view stays in place as rows are measured or arrive (see `rows`).
    let (heights, keep) = rows.plan(&keys, chat.scroll_offset);
    // A replied-to message: jump once it is loaded, paging older history (a bounded
    // number of pages) until it is; deleted or past kept history, say so.
    if let Some((id, author, pages)) = chat.seek {
        if let Some(person) = chat.quoted(&id, author).map(|found| found.person) {
            chat.jump_to = Some((person, id, 4));
            chat.flash = Some((person, id, now));
            chat.seek = None;
        } else if online && chat.loading {
            ui.ctx().request_repaint();
        } else if !online || chat.at_start || pages == 0 {
            *toast = Some((t!("home.reply_gone").into(), now + 6.0));
            chat.seek = None;
        } else {
            load_older(servers, threads, at, chat);
            chat.seek = Some((id, author, pages - 1));
            ui.ctx().request_repaint();
        }
    }
    if chat
        .flash
        .is_some_and(|(.., since)| now - since > REPLY_FLASH_SECS)
    {
        chat.flash = None;
    }
    // A message picked in the gallery: its top, a little below the view's, again for
    // a few frames as the rows around it are measured.
    if let Some((person, id, frames)) = chat.jump_to.take() {
        let at = (keys.iter()).position(|key| key.0 == person && key.1 == id);
        if let Some(at) = at {
            let top = rows.header + heights[..at].iter().sum::<f32>() - ui::SPACE_XL;
            if (top.max(0.0) - keep.unwrap_or(chat.scroll_offset)).abs() > 0.5 {
                chat.scroll_to = Some(top.max(0.0));
            }
            if frames > 0 {
                chat.jump_to = Some((person, id, frames - 1));
                ui.ctx().request_repaint();
            }
        }
    }
    let scroll_to = chat.scroll_to.take();
    let before_offset = scroll_to.or(keep).unwrap_or(chat.scroll_offset);
    let mut area = ScrollArea::vertical()
        .id_salt(("chat", key))
        .stick_to_bottom(true)
        .auto_shrink(false);
    // An offset past last frame's end (older rows were added above) would count as
    // pulled past the edge and spring back.
    let rubber_band = ui.spacing().scroll.kinetic.rubber_band;
    if let Some(offset) = keep {
        area = area.vertical_scroll_offset(offset);
        ui.spacing_mut().scroll.kinetic.rubber_band = false;
    }
    chat.index_ids();
    let settings = voice.audio.snapshot();
    let embeds = Embeds {
        servers,
        previews,
        allowed: &settings.link_previews,
    };
    let mut pick = None;
    let mut live = None;
    let mut resend = None;
    let mut reveal = None;
    let mut tops = Vec::with_capacity(keys.len());
    let (mut changed, mut drawn, mut laid_at, mut end) = (false, 0, 0.0, 0.0);
    let output = area.show_viewport(ui, |ui, viewport| {
        ui.set_width(ui.available_width());
        laid_at = viewport.min.y;
        let origin = ui.max_rect().top();
        // Scrolled there, so it no longer follows the newest message. An offset, not a
        // delta: a delta lands a frame late and asking again meanwhile would add up.
        if let Some(to) = scroll_to {
            let top = egui::pos2(ui.max_rect().left(), origin + to);
            let rect = Rect::from_min_size(top, Vec2::new(1.0, viewport.height()));
            let none = egui::style::ScrollAnimation::none();
            ui.scroll_to_rect_animation(rect, Some(Align::TOP), none);
        }
        let y = |ui: &Ui| ui.cursor().top() - origin;
        if chat.at_start {
            ui.add_space(ui::SPACE_M);
            ui.label(ui::heading(t!("home.chat_start", channel = title)));
            ui.add_space(ui::SPACE_M);
        } else if chat.loading {
            ui.vertical_centered(|ui| ui.spinner());
        }
        changed |= (y(ui) - rows.header).abs() > 0.5;
        rows.header = y(ui);
        // Half a view above and below is laid out too, so rows are measured before
        // they scroll in.
        let near = viewport.expand2(Vec2::new(0.0, viewport.height() / 2.0));
        for (i, &(row, start)) in shown.iter().enumerate() {
            let top = y(ui);
            tops.push(top);
            if top + heights[i] < near.top() || top > near.bottom() {
                ui.add_space(heights[i]);
                continue;
            }
            let msg = &chat.messages[row];
            if hidden(&chat.revealed, msg) {
                if blocked_line(ui) {
                    reveal = Some((msg.person, msg.id));
                }
                drawn += 1;
                changed |= rows.measured(keys[i], heights[i], y(ui) - top);
                continue;
            }
            // The displayed copy carries this server's emoji and sticker IDs.
            let held_on = holder(msg);
            let (message_dir, message_channel) = match (at, held_on.map(|i| &servers[i])) {
                (At::Dm(peer), Some(server)) => server
                    .dir
                    .dms
                    .iter()
                    .find(|dm| dm.peer == peer)
                    .map_or((dir, key), |dm| (&server.dir, dm.channel)),
                _ => (dir, key),
            };
            let profile = held_on
                .into_iter()
                .chain(named)
                .find(|i| servers[*i].member(&msg.person).is_some())
                .map(|i| {
                    (
                        i,
                        person::Person::with(friends, voice, &servers[i], msg.person),
                    )
                });
            let shown = message(
                ui,
                chat,
                msg,
                start,
                message_dir,
                &embeds,
                message_channel,
                profile,
                |ui| {
                    let mut shown = Vec::new();
                    if let Some(server) = held_on.map(|i| &servers[i]) {
                        // A link preview must be about one of the message's own links.
                        let links: Vec<String> = match &msg.body {
                            Body::Text(text) => markdown::cached(ui, text)
                                .links()
                                .map(String::from)
                                .collect(),
                            _ => Vec::new(),
                        };
                        let server = (&server.address, &server.commands);
                        shown = files::shown_attachments(ui, files, server, &msg.files, &links);
                    }
                    // The server isn't borrowed whole here, so look the offer up by its parts.
                    if msg.live.is_some() {
                        let action = call::chat_live(ui, &voice.live_files, dir, msg);
                        live = action
                            .map(|action| (action, msg.replicas.first().cloned()))
                            .or(live.take());
                    }
                    shown
                },
            );
            if let Some(chosen) = shown {
                pick = Some(chosen);
            }
            drawn += 1;
            changed |= rows.measured(keys[i], heights[i], y(ui) - top);
        }
        end = y(ui);
        for out in &chat.outbox {
            let start = after != Some((out.person, false));
            after = Some((out.person, false));
            if let Some(retry) = outgoing(ui, chat, out, start, dir, key, online) {
                resend = Some((out.draft.id, retry));
            }
        }
    });
    ui.spacing_mut().scroll.kinetic.rubber_band = rubber_band;
    let content = output.content_size.y;
    let offset = output.state.clamped_offset().y;
    rows.drawn = drawn;
    rows.settle(rows::Frame {
        keys: &keys,
        tops: &tops,
        end,
        content,
        view: output.inner_rect.height(),
        offset,
        moved: offset - laid_at,
        changed,
    });
    chat.rows = rows;
    if let Some(message) = reveal {
        chat.revealed.insert(message);
        ui.ctx().request_repaint();
    }
    // A row's height changed: lay it out again before showing it, so nothing jumps.
    if changed {
        ui.ctx().request_discard("chat row heights");
        ui.ctx().request_repaint();
    }
    let retry = resend.and_then(|(id, retry)| match retry {
        true => chat.retry(&id),
        false => {
            for path in chat.discard(&id).iter().flat_map(|draft| &draft.files) {
                files::discard(path);
            }
            None
        }
    });
    if let Some(draft) = retry {
        match at {
            At::Channel(index, _) => servers[index].send(Command::Send(draft)),
            At::Dm(peer) => send_dm(servers, &targets, chat, toast, now, (me, peer), draft),
        }
    }
    // `stick_to_bottom` updates the offset at the end of the pass without requesting
    // another frame; repaint once so newly scrolled-in messages and their emoji load.
    if (offset - before_offset).abs() > 0.5 {
        ui.ctx().request_repaint();
    }
    chat.scroll_offset = offset;
    // Only page once the layout has settled; the first frame always starts at the top.
    // The rows' anchor keeps the view in place as a page is added above it.
    let settled = chat.content_height == content;
    if let Some(before) = chat.keep_scroll
        && (content > before || settled)
    {
        chat.keep_scroll = None;
    }
    chat.content_height = content;
    // A DM asks each copy on its own, so one slow server doesn't hold up the others.
    if settled
        && online
        && output.state.clamped_offset().y < 4.0
        && (!chat.loading || matches!(at, At::Dm(_)))
        && !chat.at_start
        && chat.keep_scroll.is_none()
    {
        load_older(servers, threads, at, chat);
    }
    // A live file's offer lives on one server: in a DM, the one its message came from.
    let live_server = |url: Option<url::Url>| match at {
        At::Channel(index, _) => Some(index),
        At::Dm(_) => url.and_then(|url| servers.iter().position(|s| s.address == url)),
    };
    if let Some((action, url)) = live {
        handle_live_file(
            ui,
            voice,
            files,
            chat,
            live_server(url).map(|index| &servers[index]),
            action,
        );
    }
    match pick {
        Some(Pick::Reply(person, id)) => {
            chat.reply_to = Some((person, id));
            if chat.editing.take().is_some() {
                chat.clear_draft();
            }
            ui.memory_mut(|memory| memory.request_focus(composer_id(key)));
        }
        Some(Pick::JumpToReply(id, author)) => {
            chat.seek = Some((id, author, REPLY_SEEK_PAGES));
            ui.ctx().request_repaint();
        }
        Some(Pick::Edit(id, text)) => {
            chat.editing = Some(id);
            chat.reply_to = None;
            chat.open_draft(dir, &text);
            ui.memory_mut(|memory| memory.request_focus(composer_id(key)));
        }
        Some(Pick::Delete(id)) => {
            *dialog = Some(match at {
                At::Channel(index, channel) => Dialog::Delete(index, channel, id),
                At::Dm(peer) => Dialog::DeleteDm(peer, id),
            });
        }
        Some(Pick::AddReaction(id)) => {
            chat.reacting = Some(id);
            emoji_picker.open_for_reaction(ui.ctx(), composer_id(key));
        }
        Some(Pick::Poll(part)) => {
            if let Some(draft) = poll_draft(key, String::new(), part) {
                match at {
                    At::Channel(index, _) => servers[index].send(Command::Send(draft)),
                    At::Dm(peer) => send_dm(servers, &targets, chat, toast, now, (me, peer), draft),
                }
            }
        }
        Some(Pick::AddPicture(name, kind, bytes)) => {
            if let At::Channel(index, _) = at {
                add_picture(&servers[index], name, kind, &bytes);
            }
        }
        Some(Pick::Previews(id, allow)) => allow_previews(voice, previews, id, allow),
        Some(Pick::Invite(text)) => {
            follow_invite(servers, view, (joining, invite_text, net), text);
        }
        Some(Pick::Profile(index, author, pick)) => person::apply(home, index, author, pick, now),
        Some(Pick::Media(action, media)) => files.media_action(ui.ctx(), action, &media),
        None => {}
    }
    act
}

/// Add another server's emoji or sticker to this one, under a name not yet taken.
fn add_picture(server: &Server, name: String, kind: Kind, bytes: &[u8]) {
    let taken: Vec<String> = (server.dir.emoji.iter())
        .filter(|e| e.sticker == kind.sticker())
        .map(|e| e.name.clone())
        .collect();
    server.send(Command::Admin(Admin::AddEmoji {
        name: crate::emoji_zip::unique(&name, &taken),
        sticker: kind.sticker(),
        bytes: bytes.to_vec(),
        pack: None,
    }));
}

fn allow_previews(voice: &Voice, previews: &mut Previews, id: &'static str, allow: bool) {
    let mut settings = voice.audio.settings();
    settings.link_previews.insert(id.into(), allow);
    voice.audio.set_settings(settings);
    previews.allow(id, allow);
}

/// An invite link clicked in a message: its server, or the join page for it.
fn follow_invite(
    servers: &[Server],
    view: &mut View,
    (joining, invite_text, net): (&mut Join, &mut String, &Net),
    text: String,
) {
    let Ok(link) = InviteLink::parse(&text) else {
        return;
    };
    match servers.iter().position(|s| s.known == link.server) {
        Some(index) => *view = View::Server(index),
        None => {
            *view = View::Join;
            // The click is the consent to contact the server, as Continue is on the
            // join page; its preview still asks for a name and confirmation.
            if !matches!(joining, Join::Checking | Join::Joining(_)) {
                *invite_text = text;
                net.preview(link, true);
                *joining = Join::Checking;
            }
        }
    }
}

/// A poll, vote or poll end to send; `None` without randomness for its ID.
fn poll_draft(channel: Id, text: String, part: PollPart) -> Option<Draft> {
    let Ok(id) = rusttalk_core::keys::random_key() else {
        tracing::warn!("no randomness for a message ID");
        return None;
    };
    Some(Draft {
        forum_post: false,
        queued: Default::default(),
        id: *id,
        channel,
        peer: None,
        text,
        reply_to: None,
        edit_of: None,
        revision: 0,
        files: Vec::new(),
        keep: Vec::new(),
        sticker: None,
        friend: None,
        poke: false,
        poll: Some(Box::new(part)),
        voice_request: None,
        preview: None,
        inline: Vec::new(),
    })
}

/// Send a DM to every shared server that is online, with one ID so the copies merge.
/// An edit only goes where its original is.
fn send_dm(
    servers: &[Server],
    targets: &[usize],
    chat: &mut Chat,
    toast: &mut Option<(String, f64)>,
    now: f64,
    (me, peer): (Id, Id),
    draft: Draft,
) {
    let holders = draft.edit_of.and_then(|original| {
        (chat.messages.iter())
            .find(|msg| msg.id == original && msg.person == me)
            .map(|msg| &msg.replicas)
    });
    let mut sent = false;
    for server in targets.iter().map(|index| &servers[*index]) {
        if server.status != Status::Online
            || holders.is_some_and(|holders| !holders.contains(&server.address))
        {
            continue;
        }
        server.send(Command::Send(Draft {
            peer: Some(peer),
            ..draft.clone()
        }));
        sent = true;
    }
    if !sent && draft.poll.is_some() {
        *toast = Some((t!("home.waiting_try_again").into(), now + 6.0));
    } else if !sent {
        if let Some(draft) = chat.unsent(draft) {
            chat.draft = draft.text;
            chat.reply_to = draft.reply_to;
            chat.editing = draft.edit_of;
        }
        *toast = Some((t!("home.waiting_try_again").into(), now + 6.0));
    }
}

/// The stored files (and thumbnails) of message `id` that no other loaded message
/// uses. Saving a page again sends its kept pictures' references, so deleting one
/// version must leave the pictures another version still shows.
fn own_files(messages: &[Msg], id: &Id) -> Vec<Id> {
    let blobs = |msg: &Msg| -> Vec<Id> {
        (msg.files.iter())
            .flat_map(|file| std::iter::once(&file.file).chain(&file.thumbnail))
            .map(|blob| blob.id)
            .collect()
    };
    let used: HashSet<Id> = (messages.iter())
        .filter(|msg| msg.id != *id)
        .flat_map(blobs)
        .collect();
    (messages.iter())
        .filter(|msg| msg.id == *id)
        .flat_map(blobs)
        .filter(|blob| !used.contains(blob))
        .collect()
}

/// What files dragged over the chat would do: the chat dimmed behind an accent
/// frame that says where they go. On top, because the messages are drawn after it.
fn drop_zone(ui: &Ui, title: &str, max_files: usize) {
    let rect = ui.max_rect().shrink(8.0);
    let colors = ui::colors(ui);
    let accent = ui::accent(ui);
    let painter = (ui.ctx())
        .layer_painter(LayerId::new(Order::Foreground, UiId::unique("drop-zone")))
        .with_clip_rect(ui.max_rect());
    painter.rect(
        rect,
        ui::RADIUS,
        colors.main.gamma_multiply(0.92),
        Stroke::new(2.0, accent.base),
        StrokeKind::Inside,
    );
    let center = rect.center();
    painter.text(
        center - Vec2::new(0.0, 28.0),
        Align2::CENTER_CENTER,
        "📎",
        FontId::new(40.0, ui::bold_family()),
        accent.tint,
    );
    painter.text(
        center + Vec2::new(0.0, 16.0),
        Align2::CENTER_CENTER,
        t!("home.drop_to_send", channel = title),
        FontId::new(20.0, ui::bold_family()),
        colors.strong,
    );
    painter.text(
        center + Vec2::new(0.0, 42.0),
        Align2::CENTER_CENTER,
        t!("home.drop_hint", max = max_files),
        FontId::proportional(ui::TEXT_BODY),
        colors.muted,
    );
}

/// The title bar of a pane, like "# general", with a line under it.
/// In a server it also has the button that shows or hides the member list.
pub fn pane_title(
    ui: &mut Ui,
    id: &'static str,
    title: &str,
    members: Option<&mut layout::Layout>,
    files: Option<Id>,
) {
    pane_title_with(ui, id, title, files, |ui| {
        if let Some(layout) = members {
            layout::members_button(ui, layout);
        }
    });
}

fn pane_title_with(
    ui: &mut Ui,
    id: &'static str,
    title: &str,
    files: Option<Id>,
    controls: impl FnOnce(&mut Ui),
) {
    Panel::top(id)
        .frame(Frame::new().inner_margin(Margin {
            left: 4,
            right: 4,
            top: 2,
            bottom: 10,
        }))
        .show_separator_line(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                header_back(ui);
                // Buttons first, so a long name is cut short instead of pushing them off.
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    controls(ui);
                    if let Some(channel) = files {
                        files_button(ui, channel);
                        gallery::button(ui, channel);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.add(Label::new(ui::title(title)).truncate());
                    });
                });
            });
        });
}

/// A DM's title: their picture and name, which open their profile card like
/// anywhere else, and a profile button doing the same.
fn dm_title(ui: &mut Ui, home: &Home, peer: Id) -> Option<(usize, person::Pick)> {
    let name = home.peer_name(&peer);
    let index = (home.dm_server(&peer)).filter(|i| home.servers[*i].member(&peer).is_some());
    let mut pick = None;
    Panel::top("chat-title")
        .frame(Frame::new().inner_margin(Margin {
            left: 4,
            right: 4,
            top: 2,
            bottom: 10,
        }))
        .show_separator_line(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                header_back(ui);
                let Some(index) = index else {
                    ui.label(ui::title(format!("@ {name}")));
                    return;
                };
                let server = &home.servers[index];
                let target = person::Person::of(home, server, peer);
                let row = ui.scope_builder(UiBuilder::new().sense(Sense::CLICK), |ui| {
                    face(ui, &server.dir, &peer, &name, 24.0, true);
                    ui.label(ui::title(&name));
                });
                let row = row.response.on_hover_cursor(egui::CursorIcon::PointingHand);
                pick = person::interact(ui, &row, &target, false);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let size = Vec2::splat(ui::ICON_BUTTON);
                    let button =
                        ui::icon_button(ui, "👤", t!("person.profile"), ui::Look::Plain, size);
                    let card = ui::card_popup(
                        ui,
                        person::popup_id(&button, &target),
                        &button,
                        &name,
                        false,
                        |ui| target.card(ui),
                    );
                    pick = pick.take().or(card.flatten());
                    gallery::button(ui, peer);
                });
            });
        });
    pick.map(|pick| (index.unwrap_or_default(), pick))
}

enum Pick {
    /// Person and ID.
    Reply(Id, Id),
    /// Scroll to the message a reply answers: its ID and author hint.
    JumpToReply(Id, Option<Id>),
    Edit(Id, String),
    Delete(Id),
    /// Join with, or open the server of, an invite link shown as a card.
    Invite(String),
    Profile(usize, Id, person::Pick),
    /// Allow (or not) link previews from a site, by its ID.
    Previews(&'static str, bool),
    /// A vote, a poll's end or a reaction to send.
    Poll(PollPart),
    /// Open the emoji picker to react to this message.
    AddReaction(Id),
    /// Add another server's emoji or sticker to this server: name, kind and picture file.
    AddPicture(String, Kind, std::sync::Arc<Vec<u8>>),
    /// Something picked in a picture's menu.
    Media(files::MediaAction, Box<files::Media>),
}

fn composer_id(channel: Id) -> UiId {
    UiId::unique(("composer", channel))
}

/// The unsent notice: the toast whenever no other one shows. Done (`true`) only
/// once its own text was drawn.
#[cfg(any(target_os = "android", test))]
fn unsent_notice(toast: &mut Option<(String, f64)>, drawn: Option<String>, now: f64) -> bool {
    let notice = t!("home.unsent_after_close");
    if drawn.as_deref() == Some(notice) {
        return true;
    }
    if toast.is_none() {
        *toast = Some((notice.to_owned(), now + 10.0));
    }
    false
}

#[cfg(target_os = "android")]
fn drawn_toast_id() -> UiId {
    UiId::unique("drawn toast")
}

/// The toast drawn since the last time this was asked.
#[cfg(target_os = "android")]
fn drawn_toast(ctx: &egui::Context) -> Option<String> {
    ctx.data_mut(|data| data.remove_temp::<Option<String>>(drawn_toast_id()))
        .flatten()
}

/// The latest problem, in red, with a close button.
pub fn toast_line(ui: &mut Ui, toast: &mut Option<(String, f64)>) {
    if let Some((text, _)) = toast.as_ref() {
        #[cfg(target_os = "android")]
        ui.ctx()
            .data_mut(|data| data.insert_temp(drawn_toast_id(), Some(text.clone())));
        let mut close = false;
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                close = ui::small_close(ui, t!("common.dismiss")).clicked();
                ui.with_layout(Layout::left_to_right(Align::TOP), |ui| {
                    ui.add(
                        Label::new(RichText::new(text).color(ui::colors(ui).danger_text)).wrap(),
                    );
                });
            });
        });
        if close {
            *toast = None;
        }
    }
}

/// Returns a message to send, and whether a file was chosen to attach: stored (`false`) or live.
#[allow(clippy::too_many_arguments)]
fn composer(
    ui: &mut Ui,
    chat: &mut Chat,
    dir: &Directory,
    online: bool,
    channel: Id,
    hint: &str,
    (emoji_picker, scope): (&mut Picker, &picker::Scope),
    (toast, now): (&mut Option<(String, f64)>, f64),
) -> (Option<Command>, Option<bool>) {
    if let Some(editor) = &mut chat.poll_editor {
        match polls::editor(ui, editor, false) {
            Some(polls::Action::Cancel) => chat.poll_editor = None,
            Some(polls::Action::Post) if online => {
                let (question, part) = editor.poll(polls::now());
                chat.poll_editor = None;
                let draft = poll_draft(channel, question, part);
                return (draft.map(Command::Send), None);
            }
            _ => {}
        }
        return (None, None);
    }
    let mut cancel = false;
    if let Some((person, _)) = chat.reply_to {
        let author = member_name(dir, &person);
        ui.horizontal(|ui| {
            ui.label(emoji::text(t!("home.replying_to", name = author)).weak());
            cancel = ui::small_close(ui, t!("home.cancel_reply")).clicked();
        });
    }
    if chat.editing.is_some() {
        ui.horizontal(|ui| {
            ui.weak(t!("home.editing_message"));
            cancel = ui::small_close(ui, t!("home.cancel_edit")).clicked();
        });
    }
    if let Some((what, done)) = &chat.uploading {
        ui.horizontal(|ui| {
            ui.weak(t!("home.uploading", file = what));
            ui.add(
                egui::ProgressBar::new(*done)
                    .show_percentage()
                    .desired_width(200.0),
            );
        });
    }
    let wide = mentions::can_mention_everyone(dir, channel, dir.me);
    let editor = composer_id(channel);
    // From the draft as this frame starts; Enter checks the converted text below.
    let ready = !chat.draft.trim().is_empty()
        || (chat.editing.is_none() && !chat.staged.is_empty())
        || emoji_picker.has_preview();
    let mut attach = None;
    let mut send = false;
    let mut tabs = Vec::new();
    let button = Vec2::splat(COMPOSER_BUTTON);
    // On a phone, a tapped sticker waits here, above the box, until tapped or sent.
    let top = ui.cursor().top();
    let previewed = emoji_picker.preview(ui, scope);
    let bar = Frame::new()
        .fill(ui::colors(ui).composer)
        .corner_radius(ui::RADIUS_WINDOW)
        .inner_margin(Margin::same(COMPOSER_PADDING as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !chat.staged.is_empty() {
                ui.add_space(ui::SPACE_XS);
                ui.horizontal(|ui| {
                    ui.add_space(ui::SPACE_XS);
                    files::staged(ui, chat);
                });
                ui.add_space(ui::SPACE_XS);
            } else {
                files::keep_previews(ui.ctx(), &[]);
            }
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                let plus = ui
                    .add_enabled_ui(online && chat.editing.is_none(), |ui| {
                        ui::icon_button(ui, "➕", t!("home.attach_files"), Look::Ghost, button)
                    })
                    .inner;
                ui::menu_wide(&plus, |m| {
                    let upload = Item::new("📎", t!("home.upload_file"))
                        .hint(t!("home.upload_file_help"))
                        .enabled(files::PICKER);
                    if m.add(upload).clicked() {
                        attach = Some(false);
                    }
                    let live = Item::new("📡", t!("home.send_live"))
                        .hint(t!("home.send_live_help"))
                        .enabled(files::PICKER);
                    if m.add(live).on_hover_text(t!("call.direct_chat")).clicked() {
                        attach = Some(true);
                    }
                    let poll = Item::new("📊", t!("polls.create")).hint(t!("polls.create_help"));
                    if m.add(poll).clicked() {
                        chat.poll_editor = Some(Default::default());
                    }
                });
                // Room for the buttons right of the box: emoji, sticker and send.
                let right = 3.0 * (COMPOSER_BUTTON + 2.0);
                let size = Vec2::new((ui.available_width() - right).max(0.0), 0.0);
                let field = ui
                    .allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
                        composer_field(ui, chat, dir, online, channel, hint, wide, emoji_picker)
                    })
                    .inner;
                ui.add_enabled_ui(online, |ui| {
                    for (glyph, tip, tab) in [
                        ("☺", t!("home.emoji_picker"), PickerTab::Emoji),
                        ("🏷", t!("home.sticker_picker"), PickerTab::Stickers),
                    ] {
                        let look = if emoji_picker.is_open(tab) {
                            Look::Plain
                        } else {
                            Look::Ghost
                        };
                        tabs.push((ui::icon_button(ui, glyph, tip, look, button), tab));
                    }
                });
                // Dimmed until there is something to send.
                let look = if ready {
                    Look::On(ui::accent(ui).base)
                } else {
                    Look::Ghost
                };
                send = ui
                    .add_enabled_ui(online && ready, |ui| {
                        ui::icon_button(ui, "▶", t!("home.send"), look, button)
                    })
                    .inner
                    .clicked();
                field
            })
            .inner
        });
    let field = bar.inner;
    // Picking a reaction: only this server's emoji and Unicode ones can be reactions.
    let reaction_scope = picker::Scope {
        local: scope.local,
        others: Vec::new(),
    };
    let reacting = chat.reacting.filter(|_| emoji_picker.is_reacting());
    let shown_scope = if reacting.is_some() {
        &reaction_scope
    } else {
        scope
    };
    // Docked, a tap anywhere on the composer (and the sticker above it) leaves the
    // panel to the buttons and the box; on a desktop only the box keeps it open.
    let keep = if picker::docked(ui.ctx()) {
        bar.response.rect.with_min_y(top)
    } else {
        field.interact_rect
    };
    let anchor = (bar.response.rect, keep);
    let (was_open, picked) = emoji_picker.show(ui, anchor, shown_scope);
    for (response, tab) in &tabs {
        emoji_picker.button(response, *tab, editor, was_open);
    }
    // Send sends the sticker waiting above the box, rather than the text.
    let picked = picked
        .or(previewed)
        .or_else(|| send.then(|| emoji_picker.send_preview(scope)).flatten());
    // Docked, the box taking focus brings the keyboard back in the panel's place.
    emoji_picker.keyboard_took_over(ui.ctx(), editor);
    if chat.reacting.is_some() && (picked.is_some() || !emoji_picker.is_reacting()) {
        chat.reacting = None;
    }
    if let Some(target) = reacting
        && let Some(picked) = picked
    {
        // The picker offers only emoji for a reaction, all of which have a key.
        let custom = scope.local.map_or(&[][..], |dir| &dir.emoji[..]);
        let Picked::Text(sequence) = &picked else {
            return (None, None);
        };
        let Some(key) = emoji::reaction_key(sequence, custom) else {
            tracing::warn!("picked a reaction emoji without a key");
            return (None, None);
        };
        let part = reaction_vote(&target, &key, polls::now_ms(), true);
        let draft = poll_draft(channel, String::new(), part);
        return (draft.map(Command::Send), None);
    }
    // A sticker to send now: of this server by ID, or of another as a file.
    let mut sticker: Option<(Option<Id>, String, Vec<Inline>)> = None;
    match picked {
        Some(Picked::Text(sequence)) => emoji_picker.insert(&field, &mut chat.draft, &sequence),
        Some(Picked::Sticker(id, name)) => sticker = Some((Some(id), name, Vec::new())),
        Some(Picked::Foreign(emoji)) => {
            let kind = if emoji.sticker {
                Kind::Sticker
            } else {
                Kind::Emoji
            };
            let picture = (emoji.picture.as_ref())
                .filter(|picture| sticker_files::sendable(&picture.bytes, kind))
                .filter(|_| emoji::name_ok(&emoji.name));
            let known = chat.foreign.iter().any(|other| other.name == emoji.name);
            match picture {
                None => *toast = Some((t!("picker.cannot_share").into(), now + 6.0)),
                Some(picture) if emoji.sticker => {
                    let inline = Inline {
                        kind,
                        name: emoji.name.clone(),
                        bytes: picture.bytes.clone(),
                    };
                    sticker = Some((None, format!(":{}:", emoji.name), vec![inline]));
                }
                Some(_) if !known && chat.foreign.len() >= MAX_FOREIGN => {
                    *toast = Some((t!("picker.too_many_foreign").into(), now + 6.0));
                }
                Some(_) => {
                    let code = format!(":{}:", emoji.name);
                    emoji_picker.insert(&field, &mut chat.draft, &code);
                    chat.foreign.retain(|other| other.name != emoji.name);
                    chat.foreign.push(emoji);
                }
            }
        }
        None => {}
    }
    let (enter, escape, up) = ui.input(|input| {
        (
            input.key_pressed(Key::Enter) && !input.modifiers.shift,
            input.key_pressed(Key::Escape),
            input.key_pressed(Key::ArrowUp) && input.modifiers.is_none(),
        )
    });
    let focused = field.has_focus() || field.lost_focus();
    // Up in an empty box edits your last message here, like other chat apps.
    if field.has_focus() && up && chat.draft.is_empty() && chat.editing.is_none() {
        let last = chat.messages.iter().rev().find_map(|msg| match &msg.body {
            Body::Text(text)
                if msg.person == dir.me && msg.live.is_none() && msg.sticker.is_none() =>
            {
                Some((
                    msg.id,
                    markdown::cached(ui, text).map_source(text, |s| emoji::plain(s).into_owned()),
                ))
            }
            _ => None,
        });
        if let Some((id, text)) = last {
            chat.editing = Some(id);
            chat.reply_to = None;
            chat.open_draft(dir, &text);
            let end = chat.draft.chars().count();
            if let Some(mut state) = TextEdit::load_state(ui.ctx(), field.id) {
                let cursor = egui::text::CCursorRange::one(egui::text::CCursor::new(end));
                state.cursor.set_char_range(Some(cursor));
                state.store(ui.ctx(), field.id);
            }
        }
    }
    if cancel || (focused && escape) {
        chat.reply_to = None;
        if chat.editing.take().is_some() {
            chat.clear_draft();
        }
    }
    // A sticker goes out on its own, leaving the draft as it is.
    if let Some((id, name, inline)) = sticker {
        let Ok(message) = rusttalk_core::keys::random_key() else {
            tracing::warn!("no randomness for a message ID");
            return (None, attach);
        };
        let command = Command::Send(Draft {
            forum_post: false,
            queued: Default::default(),
            id: *message,
            channel,
            peer: None,
            text: name,
            reply_to: chat.reply_to.take(),
            edit_of: None,
            revision: 0,
            files: Vec::new(),
            keep: Vec::new(),
            sticker: id,
            friend: None,
            poke: false,
            poll: None,
            voice_request: None,
            preview: None,
            inline,
        });
        // Docked, the panel stays up for the next one, without the keyboard.
        if !picker::docked(ui.ctx()) {
            field.request_focus();
        }
        return (Some(command), attach);
    }
    let parser_input = chat.mentions.parser_input(&chat.draft);
    // Picked emoji of other servers come first: the one picked is the one meant.
    let known: std::borrow::Cow<[CustomEmoji]> = if chat.foreign.is_empty() {
        std::borrow::Cow::Borrowed(&dir.emoji)
    } else {
        let all = chat.foreign.iter().chain(&dir.emoji).cloned();
        std::borrow::Cow::Owned(all.collect())
    };
    let converted = markdown::cached(ui, &parser_input).map_source(&chat.draft, |s| {
        emoji::replace(&chat.mentions.encode(s), &known).into_owned()
    });
    let count = converted.chars().count();
    if count >= COUNT_FROM {
        let color = if count > MAX_TEXT_CHARS {
            ui::colors(ui).danger_text
        } else {
            ui.visuals().weak_text_color()
        };
        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
            ui.colored_label(color, format!("{count} / {MAX_TEXT_CHARS}"));
        });
    }
    let text = converted.trim();
    let allowed = mentions::allowed_to_send(&markdown::cached(ui, text), wide);
    error_text(ui, (!allowed).then_some(t!("home.no_wide_mentions")));
    // Edits change only the text; staged files wait for the next message.
    let editing = chat.editing.is_some();
    let nothing = text.is_empty() && (editing || chat.staged.is_empty());
    if !((focused && enter) || send) || nothing || count > MAX_TEXT_CHARS {
        return (None, attach);
    }
    if !allowed {
        field.request_focus();
        return (None, attach);
    }
    let Ok(id) = rusttalk_core::keys::random_key() else {
        tracing::warn!("no randomness for a message ID");
        return (None, attach);
    };
    let files = if editing {
        Vec::new()
    } else {
        chat.staged.drain(..).map(|file| file.path).collect()
    };
    let edit_of = chat.editing.take();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    let command = Command::Send(Draft {
        forum_post: false,
        queued: Default::default(),
        id: *id,
        channel,
        peer: None,
        text: text.to_owned(),
        reply_to: chat.reply_to.take(),
        edit_of,
        revision: edit_of.map_or(0, |original| chat.next_revision(dir.me, original, now_ms)),
        files,
        keep: Vec::new(),
        sticker: None,
        friend: None,
        poke: false,
        poll: None,
        voice_request: None,
        preview: None,
        // Only the picked emoji of other servers that the text still uses.
        inline: if editing {
            Vec::new()
        } else {
            (chat.foreign.iter())
                .filter(|emoji| text.contains(&emoji::token(&emoji.name, &emoji.id)))
                .filter_map(|emoji| {
                    let picture = emoji.picture.as_ref()?;
                    Some(Inline {
                        kind: Kind::Emoji,
                        name: emoji.name.clone(),
                        bytes: picture.bytes.clone(),
                    })
                })
                .collect()
        },
    });
    chat.clear_draft();
    field.request_focus();
    (Some(command), attach)
}

/// The composer's text box, with mentions and `:shortcode:` completion.
#[allow(clippy::too_many_arguments)]
fn composer_field(
    ui: &mut Ui,
    chat: &mut Chat,
    dir: &Directory,
    online: bool,
    channel: Id,
    hint: &str,
    wide: bool,
    emoji_picker: &mut Picker,
) -> Response {
    ScrollArea::vertical()
        .id_salt("composer-scroll")
        .max_height(180.0)
        .show(ui, |ui| {
            ui.add_enabled_ui(online, |ui| {
                let mut layouter = |ui: &Ui, text: &dyn egui::TextBuffer, width| {
                    let job = chat.mentions.layout(ui, text.as_str(), width, wide);
                    ui.fonts_mut(|fonts| fonts.layout_job(job))
                };
                let edit = TextEdit::multiline(&mut chat.draft)
                    .layouter(&mut layouter)
                    .frame(Frame::NONE.inner_margin(Margin::symmetric(6, COMPOSER_TEXT_PAD)))
                    .hint_text(if online && !fits(ui, hint) {
                        // A narrow bar would wrap the channel's name over lines.
                        t!("home.message_hint_short").into()
                    } else if online {
                        t!("home.message_hint", channel = hint)
                    } else {
                        t!("home.waiting_for_server").into()
                    })
                    .font(emoji::font(ui::TEXT_BODY))
                    .desired_rows(1)
                    .desired_width(f32::INFINITY)
                    .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter));
                let mut choices = Vec::new();
                let completion = ui::completion(
                    ui,
                    composer_id(channel),
                    edit,
                    |c| c != ':' && c != '@' && !emoji::shortcode_char(c) && !c.is_alphanumeric(),
                    |_, query| {
                        if query.word.starts_with('@') {
                            choices = mentions::choices(query, dir, channel, wide, &chat.mentions);
                            choices.iter().map(|choice| choice.suggestion()).collect()
                        } else {
                            picker::suggestions(query, &dir.emoji)
                        }
                    },
                );
                ui::share_with_keyboard(&completion.text_edit, false);
                if let Some(accepted) = completion.accepted
                    && !chat.mentions.accepted(&accepted.insert, choices)
                {
                    emoji_picker.remember(&accepted.insert);
                }
                completion.text_edit.response.response
            })
            .inner
        })
        .inner
}

/// Whether "Message `channel`" fits the message bar on one line.
fn fits(ui: &Ui, channel: &str) -> bool {
    let hint = t!("home.message_hint", channel = channel);
    let font = emoji::font(ui::TEXT_BODY);
    let width = ui
        .painter()
        .layout_no_wrap(hint, font, Color32::WHITE)
        .size()
        .x;
    // The field's own padding (6 each side) comes off the room.
    width + 12.0 <= ui.available_width()
}

/// The line above a reply, as in Discord: a curved line out of the avatar column to
/// the replied-to person's picture, name and a one-line preview. `true` when
/// clicked, to jump to that message.
fn reply_line(
    ui: &mut Ui,
    chat: &Chat,
    msg: &Msg,
    reply: Id,
    dir: &Directory,
    channel: Id,
) -> bool {
    let quoted = chat.quoted(&reply, msg.quoted_author);
    let person = quoted.map(|quoted| quoted.person).or(msg.quoted_author);
    let preview: String = match quoted {
        Some(quoted) if quoted.sticker.is_some() => t!("home.a_sticker").into(),
        Some(quoted) => {
            let text = mentions::Message::cached(ui, quoted, dir, channel).plain(dir);
            let snippet: String = (text.chars().take(120))
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect();
            if snippet.is_empty() {
                t!("home.a_file").into()
            } else {
                snippet
            }
        }
        None => t!("home.earlier_message").into(),
    };
    let size = Vec2::new(ui.available_width(), REPLY_AVATAR + 2.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.jump_to_reply")));
    let hovered = response.hovered();
    let c = ui::colors(ui);
    // From above the avatar up, then round to the right, ending before the picture.
    let spine = rect.left() + MESSAGE_AVATAR / 2.0;
    let start = rect.left() + MESSAGE_AVATAR + ui.spacing().item_spacing.x + ui::SPACE_XS;
    let (mid, radius) = (rect.center().y, 6.0);
    let mut points = vec![
        egui::pos2(spine, rect.bottom()),
        egui::pos2(spine, mid + radius),
    ];
    points.extend((1..=8).map(|step| {
        let angle = std::f32::consts::PI * (1.0 + step as f32 / 16.0);
        egui::pos2(spine, mid) + radius * (Vec2::new(1.0, 1.0) + Vec2::angled(angle))
    }));
    points.push(egui::pos2(start - ui::SPACE_XS, mid));
    let stroke = Stroke::new(2.0, c.border.lerp_to_gamma(c.muted, 0.3));
    ui.painter().add(Shape::line(points, stroke));
    let content = Rect::from_min_max(egui::pos2(start, rect.top()), rect.max);
    let mut row = ui.new_child(
        UiBuilder::new()
            .max_rect(content)
            .layout(Layout::left_to_right(Align::Center)),
    );
    row.spacing_mut().item_spacing.x = ui::SPACE_XS;
    let (name, text) = if hovered {
        (c.strong, c.text)
    } else {
        (c.text, c.muted)
    };
    if let Some(person) = person {
        let shown = member_name(dir, &person);
        face(&mut row, dir, &person, &shown, REPLY_AVATAR, false);
        let label = ui::bold(shown).size(ui::TEXT_CAPTION).color(name);
        row.add(quiet(label));
    }
    row.add(quiet(
        emoji::text(preview).size(ui::TEXT_CAPTION).color(text),
    ));
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

/// One line of text cut short with "…", without a tooltip repeating it.
fn quiet(text: RichText) -> Label {
    Label::new(text)
        .selectable(false)
        .truncate()
        .show_tooltip_when_elided(false)
}

/// The line in place of a blocked person's message; true when "Show" is clicked.
fn blocked_line(ui: &mut Ui) -> bool {
    ui.add_space(ui::SPACE_XS);
    let shown = ui.horizontal(|ui| {
        ui.weak("🚫");
        ui.label(RichText::new(t!("home.blocked_message")).weak().italics());
        ui.link(t!("home.show_blocked_message")).clicked()
    });
    ui.add_space(ui::SPACE_XS);
    shown.inner
}

fn message_time(ui: &mut Ui, msg: &Msg) {
    ui.label(ui::caption(time(msg.at)));
    if msg.backfilled {
        ui.label(ui::caption(t!("home.restored")));
    }
}

/// One of my messages on its way: dimmed while sending; when that failed, said so
/// with Try again and Delete. Returns `true` to send it again, `false` to delete it.
fn outgoing(
    ui: &mut Ui,
    chat: &Chat,
    out: &crate::server::Outgoing,
    start: bool,
    dir: &Directory,
    channel: Id,
    online: bool,
) -> Option<bool> {
    let msg = out.msg();
    let name = member_name(dir, &msg.person);
    let mut action = None;
    if start {
        ui.add_space(10.0);
    }
    ui.push_id(("outgoing", msg.id), |ui| {
        ui.horizontal_top(|ui| {
            if start {
                face(ui, dir, &msg.person, &name, MESSAGE_AVATAR, false);
            } else {
                ui.add_space(MESSAGE_AVATAR + ui.spacing().item_spacing.x);
            }
            ui.add_space(ui::SPACE_XS);
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = ui::SPACE_XS;
                if start {
                    ui::text_row(ui, false, |ui| ui.label(ui::bold(&name)));
                }
                ui.scope(|ui| {
                    if !out.failed {
                        ui.multiply_opacity(0.6);
                    }
                    if let Some(reply) = msg.reply_to
                        && let Some(quoted) = chat.quoted(&reply, msg.quoted_author)
                    {
                        let line = format!("↪ {}", member_name(dir, &quoted.person));
                        ui.label(emoji::text(line).size(ui::TEXT_CAPTION).weak());
                    }
                    match (&msg.body, msg.sticker) {
                        (Body::Text(name), Some(id)) => sticker(ui, dir, Some(&id), name),
                        (Body::Text(text), None) if !text.is_empty() => {
                            let mention = mentions::Message::cached(ui, &msg, dir, channel);
                            mention.doc.show(ui, dir, &mention, false, false);
                        }
                        _ => {}
                    }
                    for path in &out.draft.files {
                        ui.label(ui::caption(crate::net::file_name(path)));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    if !out.failed {
                        ui.label(ui::caption(t!("home.sending")));
                        return;
                    }
                    let failed = RichText::new(t!("home.not_sent"))
                        .size(ui::TEXT_CAPTION)
                        .color(ui::colors(ui).danger_text);
                    ui.label(failed);
                    let again =
                        ui.add_enabled_ui(online, |ui| ui::secondary(ui, t!("common.try_again")));
                    if again.inner.clicked() {
                        action = Some(true);
                    }
                    if ui::secondary(ui, t!("common.delete")).clicked() {
                        action = Some(false);
                    }
                });
            });
        });
    });
    action
}

#[allow(clippy::too_many_arguments)]
fn message(
    ui: &mut Ui,
    chat: &Chat,
    msg: &Msg,
    start: bool,
    dir: &Directory,
    embeds: &Embeds,
    channel: Id,
    profile: Option<(usize, person::Person<'_>)>,
    attachments: impl FnOnce(&mut Ui) -> Vec<files::Shown>,
) -> Option<Pick> {
    let names = |person: &Id| member_name(dir, person);
    let me = dir.me;
    if let Body::Notice {
        name,
        action,
        detail,
    } = &msg.body
    {
        // Show today's name, or the name they had if they left.
        let name = (dir.members.get(&msg.person))
            .and_then(|member| member.name.as_deref())
            .unwrap_or(name);
        ui.add_space(ui::SPACE_XS);
        ui.horizontal(|ui| {
            ui.weak("•");
            ui.label(emoji::text(notice(name, action)).weak().italics());
            ui.weak(time(msg.at));
            ui.add(
                Label::new(
                    RichText::new(t!("common.details"))
                        .small()
                        .underline()
                        .weak(),
                )
                .sense(Sense::hover()),
            )
            .on_hover_text(detail);
        });
        ui.add_space(ui::SPACE_XS);
        return None;
    }
    if let Body::Friend(signal, _) = msg.body {
        ui.add_space(ui::SPACE_XS);
        ui.horizontal(|ui| {
            ui.weak("•");
            let line = friends::line(signal, &names(&msg.person));
            ui.label(emoji::text(line).weak().italics());
            message_time(ui, msg);
        });
        ui.add_space(ui::SPACE_XS);
        return None;
    }
    let mention = mentions::Message::cached(ui, msg, dir, channel);
    let my_roles = (dir.members.get(&me)).map_or(&[][..], |member| member.roles.as_slice());
    let pinged = mentions::mentions_me(&mention, &me, my_roles);
    let background = ui.painter().add(Shape::Noop);
    let name = names(&msg.person);
    let mut invite = None;
    if start {
        ui.add_space(10.0);
    }
    // The row senses clicks behind its contents, for the right-click menu. Not a Tab
    // stop: the hover toolbar has the same actions as buttons.
    let builder = UiBuilder::new()
        .scope_id(ui.make_persistent_id((channel, msg.person, msg.id)))
        .sense(Sense::CLICK);
    let mut trigger = None;
    let mut media = Vec::new();
    let row = ui.scope_builder(builder, |ui| {
        ui.spacing_mut().item_spacing.y = ui::SPACE_XS;
        // Above the avatar, as in Discord, so the avatar lines up with the name.
        if let Some(reply) = msg.reply_to
            && reply_line(ui, chat, msg, reply, dir, channel)
        {
            invite = Some(Pick::JumpToReply(reply, msg.quoted_author));
        }
        ui.horizontal_top(|ui| {
            if start {
                let avatar = face(ui, dir, &msg.person, &name, MESSAGE_AVATAR, false);
                if profile.is_some() {
                    let button = avatar.interact(Sense::click());
                    trigger = Some(person::profile_trigger(ui, button, &name));
                }
            } else {
                ui.add_space(MESSAGE_AVATAR + ui.spacing().item_spacing.x);
            }
            ui.add_space(ui::SPACE_XS);
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                if start || msg.backfilled {
                    // At the avatar's top edge, the text right below.
                    ui::text_row(ui, false, |ui| {
                        if start {
                            if let Some(avatar) = trigger.take() {
                                trigger = Some(avatar.union(person::profile_name(ui, &name)));
                            } else {
                                ui.label(ui::bold(&name));
                            }
                        }
                        message_time(ui, msg);
                    });
                }
                match &msg.body {
                    Body::Text(name) if msg.sticker.is_some() => {
                        sticker(ui, dir, msg.sticker.as_ref(), name);
                    }
                    Body::Poll(question, poll) => {
                        let tally = chat.tally(msg).unwrap_or_default();
                        if let Some(choices) =
                            polls::card(ui, question, poll, &tally, &me, names, None)
                        {
                            invite = Some(Pick::Poll(PollPart::Vote {
                                poll: msg.id,
                                sent: polls::now_ms(),
                                choices,
                            }));
                        }
                    }
                    Body::Text(text) => {
                        // Another server's sticker is its picture alone, drawn with the files.
                        let sticker = crate::sticker_files::sticker_of(&msg.files, text);
                        if !text.is_empty() && sticker.is_none() {
                            ui.vertical(|ui| {
                                let jumbo = emoji::jumbo(text)
                                    && msg.files.is_empty()
                                    && msg.live.is_none();
                                mention.doc.show(
                                    ui,
                                    dir,
                                    &mention,
                                    jumbo,
                                    chat.edited.contains_key(&(msg.person, msg.id)),
                                );
                                for link in mention.doc.invites() {
                                    if invite_card(ui, embeds.servers, link) {
                                        invite = Some(Pick::Invite(link.clone()));
                                    }
                                }
                                for (provider, key) in previews::supported(mention.doc.links()) {
                                    if let Some(pick) = link_preview(ui, embeds, provider, &key) {
                                        invite = Some(pick);
                                    }
                                }
                            });
                        }
                        media = attachments(ui);
                    }
                    _ => {
                        ui.label(
                            RichText::new(t!("home.message_unreadable"))
                                .italics()
                                .weak(),
                        );
                    }
                }
                if matches!(msg.body, Body::Text(_) | Body::Poll(..))
                    && let Some(chosen) = reaction_chips(ui, chat, msg, dir)
                {
                    invite = Some(chosen);
                }
            });
        });
    });
    let rect = row.response.rect;
    let mine = msg.person == me;
    let text = match &msg.body {
        Body::Text(text) => Some(text.as_str()),
        _ => None,
    };
    let edit_text = |text: &str| {
        mention
            .doc
            .map_source(text, |s| emoji::plain(s).into_owned())
    };
    // Previewing the server as other roles: nothing to change.
    let live = !dir.perms.preview;
    let can_reply = live && text.is_some();
    let can_react =
        live && matches!(msg.body, Body::Text(_) | Body::Poll(..)) && chat.poll_editor.is_none();
    let can_end_poll = live
        && mine
        && matches!(msg.body, Body::Poll(..))
        && chat
            .tally(msg)
            .is_some_and(|tally| tally.open(polls::now()));
    let can_edit = live && mine && text.is_some() && msg.sticker.is_none();
    // A DM copy that didn't open proves nothing about who wrote it, and deleting a
    // DM message reaches every server, so only a readable copy of mine can be deleted.
    let dm_copy = !msg.replicas.is_empty();
    let authored = mine && !(dm_copy && msg.body == Body::Unreadable);
    let can_delete = live
        && (authored
            || (dir.perms.delete_messages && !dir.dms.iter().any(|dm| dm.channel == channel)));
    let mut pick = invite;
    if let (Some(trigger), Some((index, profile))) = (&trigger, profile)
        && let Some(action) = person::profile(ui, trigger, &profile, false)
    {
        pick = Some(Pick::Profile(index, msg.person, action));
    }
    // Emoji and stickers of other servers in the message, for people who may add them.
    let in_dm = dir.dms.iter().any(|dm| dm.channel == channel);
    let addable: Vec<(Kind, String, Id)> = if dir.perms.manage_emoji && !in_dm {
        (msg.files.iter())
            .filter_map(|file| {
                let (kind, name) = sticker_files::of(file)?;
                Some((kind, name, file.file.id))
            })
            .collect()
    } else {
        Vec::new()
    };
    // This server's sticker is the whole message; its picture is here already.
    let sticker_picture = (msg.sticker.as_ref())
        .filter(|_| matches!(msg.body, Body::Text(_)) && files::COPY_IMAGE)
        .and_then(|id| dir.emoji_by_id(id))
        .and_then(|emoji| emoji.picture.as_ref());
    // The menu of the message, and of each picture in it with the picture's items on
    // top. Quick reactions would go first, above both.
    let menu = |m: &mut ui::Menu, media: Option<&files::Media>| {
        let mut pick = None;
        if let Some(media) = media {
            if let Some(action) = files::media_items(m, media) {
                pick = Some(Pick::Media(action, Box::new(media.clone())));
            }
            m.separator();
        } else if let Some(picture) = sticker_picture {
            if m.item("🖼", t!("files.copy_image")).clicked() {
                m.ui().ctx().copy_image(picture.first.0.clone());
            }
            m.separator();
        }
        if can_reply && m.item("↩", t!("common.reply")).clicked() {
            pick = Some(Pick::Reply(msg.person, msg.id));
        }
        if can_react && m.item("☺", t!("home.add_reaction")).clicked() {
            pick = Some(Pick::AddReaction(msg.id));
        }
        if can_edit
            && let Some(text) = &text
            && m.item("✏", t!("common.edit")).clicked()
        {
            pick = Some(Pick::Edit(msg.id, edit_text(text)));
        }
        if can_reply && m.item("📋", t!("home.copy_text")).clicked() {
            m.ui().ctx().copy_text(mention.plain(dir));
        }
        if can_end_poll && m.item("🏁", t!("polls.end")).clicked() {
            pick = Some(Pick::Poll(PollPart::End { poll: msg.id }));
        }
        for (kind, name, blob) in &addable {
            let label = match kind {
                Kind::Sticker => t!("home.add_sticker_here").to_owned(),
                Kind::Emoji => t!("home.add_emoji_here", name = name),
            };
            if let Some(picture) = sticker_files::picture(m.ui().ctx(), blob)
                && m.item("➕", &label).clicked()
            {
                pick = Some(Pick::AddPicture(name.clone(), *kind, picture.bytes));
            }
        }
        if m.item("🆔", t!("home.copy_message_id")).clicked() {
            m.ui().ctx().copy_text(hex::encode(msg.id));
        }
        if can_delete {
            m.separator();
            if m.danger("🗑", t!("common.delete")).clicked() {
                pick = Some(Pick::Delete(msg.id));
            }
        }
        pick
    };
    // Pictures first: on a long press the first menu to ask opens.
    for shown in &media {
        if let Some(Some(chosen)) =
            ui::context_menu(&shown.response, |m| menu(m, Some(&shown.media)))
        {
            pick = Some(chosen);
        }
    }
    let anchor = trigger.map_or_else(|| row.response.clone(), |button| row.response.union(button));
    if let Some(Some(chosen)) = ui::context_menu(&anchor, |m| menu(m, None)) {
        pick = Some(chosen);
    }
    // The toolbar is a mouse feature. On a touch screen a tap does nothing here and a
    // long press opens the menu above; egui keeps the finger as a hover after a tap.
    let hovered = !ui::TOUCH && !ui.input(|i| i.any_touches()) && ui.rect_contains_pointer(rect);
    // While its menu is open the row stays lit, so it is clear which message it is for.
    let menu_open =
        ui::menu_open(&anchor) || media.iter().any(|shown| ui::menu_open(&shown.response));
    let lit = hovered || menu_open;
    if pinged {
        ui::mention_highlight(ui, background, rect.expand(2.0), lit);
    } else if lit {
        ui.painter().set(
            background,
            Shape::rect_filled(rect.expand(2.0), ui::RADIUS_S, ui::colors(ui).hover_soft),
        );
    }
    // Jumped to from a reply: an accent fill that fades out.
    if let Some((.., since)) = chat.flash.filter(|f| f.0 == msg.person && f.1 == msg.id) {
        let left = 1.0 - (ui.input(|i| i.time) - since) / REPLY_FLASH_SECS;
        if left > 0.0 {
            let c = ui::colors(ui);
            let surface = if lit { c.hover_soft } else { c.main };
            let fill = surface.lerp_to_gamma(ui::accent(ui).base, 0.3 * left.min(1.0) as f32);
            ui.painter().set(
                background,
                Shape::rect_filled(rect.expand(2.0), ui::RADIUS_S, fill),
            );
            ui.ctx().request_repaint();
        }
    }
    if !hovered {
        return pick;
    }
    let buttons = [can_react, can_reply, can_edit, can_delete]
        .into_iter()
        .filter(|on| *on)
        .count() as f32;
    if buttons == 0.0 {
        return pick;
    }
    let button = TOOLBAR_BUTTON;
    let size = Vec2::new(
        buttons * button.x + (buttons - 1.0) * 2.0 + 8.0,
        button.y + 8.0,
    );
    let top = rect.top() - 14.0;
    let bar = Rect::from_min_size(egui::pos2(rect.right() - size.x - 8.0, top), size);
    let c = ui::colors(ui);
    ui.painter()
        .add(ui.visuals().popup_shadow.as_shape(bar, ui::RADIUS));
    ui.painter().rect(
        bar,
        ui::RADIUS,
        c.raised,
        egui::Stroke::new(1.0, c.border),
        egui::StrokeKind::Inside,
    );
    let mut ui = ui.new_child(
        UiBuilder::new()
            .max_rect(bar.shrink(4.0))
            .layout(Layout::left_to_right(Align::Center)),
    );
    ui.spacing_mut().item_spacing.x = 2.0;
    if can_react
        && ui::icon_button(&mut ui, "☺", t!("home.add_reaction"), Look::Ghost, button).clicked()
    {
        pick = Some(Pick::AddReaction(msg.id));
    }
    if can_reply && ui::icon_button(&mut ui, "↩", t!("common.reply"), Look::Ghost, button).clicked()
    {
        pick = Some(Pick::Reply(msg.person, msg.id));
    }
    if can_edit
        && let Some(text) = &text
        && ui::icon_button(&mut ui, "✏", t!("common.edit"), Look::Ghost, button).clicked()
    {
        pick = Some(Pick::Edit(msg.id, edit_text(text)));
    }
    if can_delete
        && ui::icon_button(&mut ui, "🗑", t!("common.delete"), Look::Ghost, button).clicked()
    {
        pick = Some(Pick::Delete(msg.id));
    }
    pick
}

/// The reactions under a message: each emoji and how many, lit when one is mine. A click
/// adds or takes back mine; hovering, a right-click or a long press shows who reacted.
fn reaction_chips(ui: &mut Ui, chat: &Chat, msg: &Msg, dir: &Directory) -> Option<Pick> {
    let reactions = chat.reactions(&msg.id, |key| emoji::reacted(key, &dir.emoji).is_some());
    if reactions.is_empty() {
        return None;
    }
    let mut pick = None;
    ui.add_space(ui::SPACE_XS);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(ui::SPACE_XS);
        for (key, people) in &reactions {
            let Some(shown) = emoji::reacted(key, &dir.emoji) else {
                continue;
            };
            let mine = people.contains(&dir.me);
            let c = ui::colors(ui);
            let count = RichText::new(people.len().to_string())
                .size(ui::TEXT_CAPTION)
                .color(if mine { c.strong } else { c.text });
            let response = match shown {
                emoji::Reacted::Unicode(sequence) => {
                    ui::reaction_chip(ui, mine, (emoji::text(sequence), count))
                }
                emoji::Reacted::Custom(custom) => {
                    let size = Vec2::splat(ui::TEXT_BODY + 4.0);
                    let image = match custom.picture.clone() {
                        Some(picture) => egui::Atom::paint(size, move |ui, args| {
                            paint_hovered(ui, args.rect, &picture, 0.0);
                        }),
                        None => ui::caption(&custom.name).into(),
                    };
                    ui::reaction_chip(ui, mine, (image, count))
                }
            };
            let names = (people.iter().map(|p| member_name(dir, p)))
                .collect::<Vec<_>>()
                .join(", ");
            let who = t!("home.reacted_with", names = &names, emoji = shown.name());
            response.widget_info(|| WidgetInfo::selected(Role::Button, true, mine, &who));
            let response = response.on_hover_ui(|ui| who_reacted(ui, shown, &names));
            if response.clicked() {
                let part = reaction_vote(&msg.id, key, polls::now_ms(), !mine);
                pick = Some(Pick::Poll(part));
            }
            ui::context_menu(&response, |m| {
                m.ui().horizontal(|ui| {
                    ui.add_space(10.0);
                    who_reacted(ui, shown, &names);
                });
            });
        }
    });
    pick
}

/// Who reacted, led by the emoji drawn as in the chip (a text glyph would fall back
/// to the font's plain look), for the hover tip and the right-click or long-press card.
fn who_reacted(ui: &mut Ui, shown: emoji::Reacted, names: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = ui::SPACE_S;
        let size = ui::TEXT_BODY + 4.0;
        match shown {
            emoji::Reacted::Unicode(sequence) => {
                ui.label(emoji::text(sequence).size(size));
            }
            emoji::Reacted::Custom(custom) => match &custom.picture {
                Some(picture) => {
                    emoji_image(ui, picture, size, &shown.name());
                }
                None => {
                    ui.label(ui::caption(shown.name()));
                }
            },
        }
        ui.add(egui::Label::new(t!("home.reacted", names = names)).wrap());
    });
}

/// A RustTalk invite under a message. Shows only what the link carries, or what this
/// device already knows about a server it is in: viewing it contacts nothing.
/// Returns whether Join or Open was clicked.
fn invite_card(ui: &mut Ui, servers: &[Server], link: &str) -> bool {
    let Ok(invite) = InviteLink::parse(link) else {
        return false;
    };
    let address = &invite.server.address;
    let place = match address.port() {
        Some(port) => format!("{}:{port}", host(address)),
        None => host(address),
    };
    // Same address but other pins: maybe a rebuilt server, maybe not the one you
    // joined. Joining checks it, so it gets the normal Join card.
    let known = servers.iter().find(|server| server.known == invite.server);
    let (image, title, description, button) = match known {
        Some(server) => (
            match &server.dir.icon {
                Some(icon) => embed::Image::Picture(icon),
                None => embed::Image::Glyph(initials(server.name())),
            },
            server.name(),
            Some(place.as_str()),
            embed::Button {
                label: t!("home.invite_open"),
                primary: false,
            },
        ),
        None => (
            embed::Image::Glyph("🔗".into()),
            place.as_str(),
            None,
            embed::Button {
                label: t!("common.join"),
                primary: true,
            },
        ),
    };
    embed::Embed {
        image: Some(image),
        kind: t!("home.invite_card"),
        title,
        description,
        fields: &[],
        buttons: &[button],
    }
    .show(ui)
    .is_some()
}

/// A link's preview card, from a site the person allowed, or the question whether to
/// allow it. Nothing is fetched before that yes, nor for messages off screen.
fn link_preview(
    ui: &mut Ui,
    embeds: &Embeds,
    provider: &'static dyn previews::Provider,
    key: &str,
) -> Option<Pick> {
    let site = provider.name();
    match embeds.allowed.get(provider.id()) {
        Some(false) => None,
        None => {
            let ask = t!("previews.ask", site = site);
            let help = t!("previews.ask_help", site = site);
            let clicked = embed::Embed {
                image: None,
                kind: site,
                title: &ask,
                description: Some(&help),
                fields: &[],
                buttons: &[
                    embed::Button {
                        label: t!("previews.allow"),
                        primary: true,
                    },
                    embed::Button {
                        label: t!("previews.not_now"),
                        primary: false,
                    },
                ],
            }
            .show(ui)?;
            Some(Pick::Previews(provider.id(), clicked == 0))
        }
        Some(true) => {
            let on_screen = ui.clip_rect().contains(ui.next_widget_position());
            let preview = if on_screen {
                embeds.previews.get(provider, key)
            } else {
                embeds.previews.cached(provider, key)
            }?;
            let details = &preview.details;
            let fields: Vec<_> = (details.fields.iter())
                .map(|(field, value)| (field.label(), value.as_str()))
                .collect();
            let view = provider.action();
            let kind = if details.nsfw {
                format!("{site} · {}", t!("previews.nsfw"))
            } else {
                site.to_owned()
            };
            let texture = preview.texture(ui.ctx());
            let clicked = embed::Embed {
                image: texture.map(if details.icon {
                    embed::Image::Icon
                } else {
                    embed::Image::Wide
                }),
                kind: &kind,
                title: &details.title,
                description: (!details.description.is_empty()).then_some(&*details.description),
                fields: &fields,
                buttons: &[embed::Button {
                    label: &view,
                    primary: false,
                }],
            }
            .show(ui);
            if clicked.is_some() {
                ui.ctx()
                    .open_url(egui::OpenUrl::new_tab(details.url.clone()));
            }
            None
        }
    }
}

/// A sticker message: the picture, large, or its name when not available.
fn sticker(ui: &mut Ui, dir: &Directory, id: Option<&Id>, name: &str) {
    let code = format!(":{name}:");
    match id
        .and_then(|id| dir.emoji_by_id(id))
        .and_then(|e| e.picture.as_ref())
    {
        Some(picture) => {
            emoji_image(ui, picture, STICKER_SIZE, &t!("home.sticker", code = code))
                .on_hover_text(code);
        }
        None => {
            ui.label(
                emoji::text(t!("home.sticker", code = code))
                    .italics()
                    .weak(),
            );
        }
    }
}

fn dialog_title(home: &Home, dialog: &Dialog) -> String {
    match dialog {
        Dialog::ServerName(..) => t!("home.server_name").into(),
        Dialog::MyName(..) => t!("home.your_name_dialog").into(),
        Dialog::Category(..) => t!("home.create_category").into(),
        Dialog::Channel { .. } => t!("home.create_channel").into(),
        Dialog::EditChannel { .. } => t!("home.edit_channel").into(),
        Dialog::RenameCategory(..) => t!("home.rename_category").into(),
        Dialog::DeleteItem(_, reorder::Item::Channel(_), _) => {
            t!("home.delete_channel_title").into()
        }
        Dialog::DeleteItem(_, reorder::Item::Category(_), _) => {
            t!("home.delete_category_title").into()
        }
        Dialog::Invite(invite) => {
            t!(
                "home.invite_to",
                server = home.servers[invite.server].name()
            )
        }
        Dialog::Member(index, person) => home.servers[*index].member_name(person),
        Dialog::Delete(..) | Dialog::DeleteDm(..) => t!("home.delete_message_title").into(),
        Dialog::Unshare(..) => t!("files.remove_title").into(),
        Dialog::Leave(index, _) => t!("home.leave_title", server = home.servers[*index].name()),
        Dialog::Poke(person, _) => t!("pokes.poke_title", name = home.peer_name(person)),
    }
}

fn dialogs(ctx: &egui::Context, home: &mut Home) {
    let Some(dialog) = &home.dialog else {
        return;
    };
    let managed = match dialog {
        Dialog::EditChannel { server, .. }
        | Dialog::RenameCategory(server, ..)
        | Dialog::DeleteItem(server, ..) => Some(*server),
        _ => None,
    };
    if managed.is_some_and(|index| !home.servers[index].dir.perms.manage_channels) {
        home.dialog = None;
        return;
    }
    let title = dialog_title(home, dialog);
    let Some(dialog) = &mut home.dialog else {
        return;
    };
    let mut close = false;
    let mut command = None;
    let mut delete_dm = None;
    let mut delete_channel = None;
    let mut poke = None;
    // Message (`None`) or a friend action, from a person's details.
    let mut person_action = None;
    // A server's own name, or `None` for "Use my default name".
    let mut name_choice: Option<(usize, Option<String>)> = None;
    let default_name = home.voice.audio.settings().display_name;
    let ((), dismissed) = ui::dialog(ctx, "dialog", &title, |ui| match dialog {
        Dialog::ServerName(index, name) => {
            ui.label(t!("home.server_name_help"));
            if text_dialog(ui, name, t!("common.save"), &mut close) {
                command = Some((*index, Command::SetServerName(name.trim().to_owned())));
            }
        }
        Dialog::MyName(index, name) => {
            ui.label(t!("home.my_name_help"));
            let (_, submitted) = name_field(ui, name);
            if !default_name.is_empty() {
                ui.add_space(ui::SPACE_S);
                let label = t!("home.use_default_name", name = default_name);
                if ui::secondary(ui, &label).clicked() {
                    name_choice = Some((*index, None));
                }
            }
            if text_footer(ui, t!("common.save"), submitted, &mut close) && !name.trim().is_empty()
            {
                name_choice = Some((*index, Some(name.trim().to_owned())));
            }
        }
        Dialog::Category(index, name) => {
            if text_dialog(ui, name, t!("common.create"), &mut close) {
                command = Some((*index, Command::CreateCategory(name.trim().to_owned())));
            }
        }
        Dialog::Channel {
            server,
            name,
            kind,
            category,
            new_category,
        } => {
            let was = *kind;
            ui.horizontal(|ui| {
                ui.selectable_value(
                    kind,
                    ChannelKind::Text,
                    format!("# {}", t!("home.kind_text")),
                );
                ui.selectable_value(
                    kind,
                    ChannelKind::Voice,
                    format!("🔊 {}", t!("home.kind_voice")),
                );
                ui.selectable_value(
                    kind,
                    ChannelKind::Page,
                    format!("{} {}", kind_icon(ChannelKind::Page), t!("home.kind_page")),
                );
                ui.selectable_value(
                    kind,
                    ChannelKind::Forum,
                    format!("💬 {}", t!("home.kind_forum")),
                );
            });
            let help = match kind {
                ChannelKind::Page => Some(t!("home.kind_page_help")),
                ChannelKind::Forum => Some(t!("home.kind_forum_help")),
                _ => None,
            };
            if let Some(help) = help {
                ui.weak(help);
            }
            let data = &home.servers[*server];
            // Switching kind moves a default pick along, but keeps one the person made.
            if *kind != was && *category == default_category(data, was) {
                *category = default_category(data, *kind);
                if *new_category == category_name(was) {
                    *new_category = category_name(*kind).into();
                }
            }
            let categories: Vec<_> = data
                .dir
                .categories
                .iter()
                .filter_map(|c| Some((c.id?, c.name.clone())))
                .collect();
            if !categories.is_empty() {
                let current = categories
                    .iter()
                    .find(|(id, _)| Some(*id) == *category)
                    .map_or(t!("home.new_category"), |(_, name)| name.as_str());
                egui::ComboBox::from_label(t!("home.category"))
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for (id, name) in &categories {
                            ui.selectable_value(category, Some(*id), name.as_str());
                        }
                        ui.selectable_value(category, None, t!("home.new_category"));
                    });
            }
            if category.is_none() {
                let label = ui.label(t!("home.new_category_name"));
                ui.add(ui::field(new_category).desired_width(f32::INFINITY))
                    .labelled_by(label.id);
            }
            ui.label(t!("common.name"));
            if text_dialog(ui, name, t!("common.create"), &mut close)
                && (category.is_some() || !new_category.trim().is_empty())
            {
                command = Some((
                    *server,
                    Command::CreateChannel {
                        category: *category,
                        new_category: new_category.trim().to_owned(),
                        kind: *kind,
                        name: name.trim().to_owned(),
                    },
                ));
            }
        }
        Dialog::EditChannel {
            server,
            id,
            tab,
            name,
            topic,
            topic_changed,
            audio,
            limit,
            flags,
        } => {
            let data = &home.servers[*server];
            let Some(channel) = data.channel(id) else {
                ui.label(t!("admin.channel_deleted"));
                close |= ui::secondary(ui, t!("common.close")).clicked();
                return;
            };
            ui.add_space(ui::SPACE_S);
            ui::tabs(
                ui,
                tab,
                &[
                    (ChannelTab::General, t!("admin.group_general")),
                    (ChannelTab::Permissions, t!("admin.permissions")),
                ],
            );
            let mut entered = false;
            let top = ui.cursor().top();
            match tab {
                ChannelTab::General => {
                    let label = ui.label(t!("home.channel_name")).id;
                    let (name_field, name_entered) = name_field(ui, name);
                    name_field.labelled_by(label);
                    let label = ui.label(t!("home.topic_optional")).id;
                    let field = ui
                        .add(
                            ui::field(topic)
                                .char_limit(250)
                                .desired_width(f32::INFINITY),
                        )
                        .labelled_by(label);
                    *topic_changed |= field.changed();
                    if let Some((kbps, changed)) = audio {
                        ui.label(t!("home.audio_quality", kbps = *kbps));
                        ui.spacing_mut().slider_width = ui.available_width();
                        let slider = ui.add(
                            egui::Slider::new(kbps, AUDIO_KBPS)
                                .show_value(false)
                                .integer(),
                        );
                        slider.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::Role::Slider,
                                true,
                                t!("home.audio_quality_label"),
                            )
                        });
                        *changed |= slider.changed();
                        ui.label(ui::caption(t!("home.audio_quality_help")));
                    }
                    if let Some((most, changed)) = limit {
                        let title = t!("home.member_limit");
                        let spin = ui::setting_row(ui, title, t!("home.member_limit_help"), |ui| {
                            let spin = ui.add(
                                DragValue::new(most)
                                    .range(MEMBER_LIMIT)
                                    .custom_formatter(|value, _| match value as u32 {
                                        0 => t!("home.no_limit").into(),
                                        value => value.to_string(),
                                    })
                                    .custom_parser(no_limit_parser),
                            );
                            spin.widget_info(|| WidgetInfo::labeled(Role::SpinButton, true, title));
                            spin
                        });
                        *changed |= spin.changed();
                    }
                    entered = name_entered || submitted(ui, &field);
                }
                ChannelTab::Permissions => {
                    admin::channel_flags(ui, data, channel, flags, CHANNEL_TAB_HEIGHT);
                }
            }
            // The same height on both tabs, so the centred dialog doesn't jump on a switch.
            ui.add_space((CHANNEL_TAB_HEIGHT - (ui.cursor().top() - top)).max(0.0));
            let mut save = entered;
            ui::footer(ui, |ui| {
                save |= ui::primary(ui, t!("common.save")).clicked();
                close |= ui::secondary(ui, t!("common.cancel")).clicked();
                if *tab == ChannelTab::General {
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        if ui::danger(ui, t!("home.delete_channel")).clicked() {
                            delete_channel = Some((*server, *id));
                        }
                    });
                }
            });
            if save && !name.trim().is_empty() {
                // Only what changed, so saving permissions doesn't log a rename too.
                let audio_kbps = audio.and_then(|(kbps, changed)| changed.then_some(kbps));
                let member_limit = limit.and_then(|(most, changed)| changed.then_some(most));
                let voice_changed = audio_kbps.is_some() || member_limit.is_some();
                if name.trim() != channel.name || *topic_changed || voice_changed {
                    command = Some((
                        *server,
                        Command::RenameChannel {
                            id: *id,
                            name: name.trim().to_owned(),
                            topic: topic_changed.then(|| topic.trim().to_owned()),
                            audio_kbps,
                            member_limit,
                        },
                    ));
                }
                for (role, role_flags) in flags.drain(..) {
                    if role_flags != admin::channel_role_flags(channel, &role) {
                        let change = Admin::ChannelFlags(*id, role, role_flags);
                        data.send(Command::Admin(change));
                    }
                }
                close = true;
            }
        }
        Dialog::RenameCategory(index, id, name) => {
            let label = ui.label(t!("home.category_name")).id;
            let (field, submitted) = name_field(ui, name);
            field.labelled_by(label);
            if text_footer(ui, t!("common.save"), submitted, &mut close) && !name.trim().is_empty()
            {
                command = Some((*index, Command::RenameCategory(*id, name.trim().to_owned())));
            }
        }
        Dialog::DeleteItem(index, item, name) => {
            let (prompt, detail, delete) = match *item {
                reorder::Item::Channel(id) => (
                    t!("home.delete_channel_prompt", name = name),
                    t!("home.delete_channel_help"),
                    Command::DeleteChannel(id),
                ),
                reorder::Item::Category(id) => (
                    t!("home.delete_category_prompt", name = name),
                    t!("home.delete_category_help"),
                    Command::DeleteCategory(id),
                ),
            };
            ui.label(prompt);
            ui.label(detail);
            ui::footer(ui, |ui| {
                if ui::danger(ui, t!("common.delete")).clicked() {
                    command = Some((*index, delete));
                }
                close |= ui::secondary(ui, t!("common.cancel")).clicked();
            });
        }
        Dialog::Invite(invite) => {
            let server = &home.servers[invite.server];
            match &invite.link {
                None => {
                    let uses = [
                        (1, t!("home.uses_1")),
                        (10, t!("home.uses_10")),
                        (1000, t!("home.uses_unlimited")),
                    ];
                    let times = [
                        (60 * 60, t!("home.for_1_hour")),
                        (DAY, t!("home.for_1_day")),
                        (DAY * 7, t!("home.for_7_days")),
                        (DAY * 30, t!("home.for_30_days")),
                    ];
                    ui.label(ui::bold(t!("home.who_can_use")));
                    ui.horizontal(|ui| {
                        for (value, label) in uses {
                            ui.selectable_value(&mut invite.uses, value, label);
                        }
                    });
                    ui.label(ui::bold(t!("home.stops_after")));
                    ui.horizontal(|ui| {
                        for (value, label) in times {
                            ui.selectable_value(&mut invite.seconds, value, label);
                        }
                    });
                    ui::footer(ui, |ui| {
                        if invite.making {
                            ui.spinner();
                        } else if ui::primary(ui, t!("home.create_link")).clicked() {
                            invite.making = true;
                            server.send(Command::Invite(invite.uses, invite.seconds));
                        }
                        close |= ui::secondary(ui, t!("common.cancel")).clicked();
                    });
                }
                Some((id, link)) => {
                    ui.label(t!("home.send_link"));
                    let mut shown = link.as_str();
                    ui.add(ui::field(&mut shown).desired_width(f32::INFINITY));
                    ui::footer(ui, |ui| {
                        if ui::primary(ui, t!("home.copy_link")).clicked() {
                            ui.ctx().copy_text(link.clone());
                        }
                        if ui::secondary(ui, t!("home.stop_link")).clicked() {
                            command = Some((
                                invite.server,
                                Command::Admin(Admin::RevokeInvite(id.clone())),
                            ));
                        }
                        close |= ui::secondary(ui, t!("common.done")).clicked();
                    });
                }
            }
        }
        Dialog::Member(index, person) => {
            let server = &home.servers[*index];
            let other = *person != server.dir.me;
            if other {
                let mutual: Vec<&str> = (home.servers.iter())
                    .filter(|server| server.member(person).is_some())
                    .map(Server::name)
                    .collect();
                ui.label(t!("friends.shared_servers", servers = mutual.join(", ")));
                ui.add_space(6.0);
            }
            ui.label(t!("home.compare_ids"));
            ui.add_space(6.0);
            ui.label(t!("home.detail_id", fingerprint = fingerprint(person)));
            if let Some(member) = server.dir.members.iter().find(|m| m.person == *person) {
                ui::section(ui, t!("home.devices"));
                for device in &member.devices {
                    ui.label(format!("{}  {}", device.name, device.fingerprint));
                }
            }
            ui::footer(ui, |ui| {
                if other {
                    if ui::primary(ui, t!("home.message")).clicked() {
                        person_action = Some((*person, None));
                    }
                    let ask = match home.friends.relation(person) {
                        Relation::None => Some((t!("friends.add"), Act::Add)),
                        Relation::Incoming => Some((t!("friends.accept_request"), Act::Accept)),
                        _ => None,
                    };
                    if let Some((label, act)) = ask
                        && ui::secondary(ui, label).clicked()
                    {
                        person_action = Some((*person, Some(act)));
                    }
                }
                close |= ui::secondary(ui, t!("common.close")).clicked();
            });
        }
        Dialog::Delete(index, channel, id) => {
            ui.label(t!("home.delete_message_help"));
            ui::footer(ui, |ui| {
                if ui::danger(ui, t!("common.delete")).clicked() {
                    let chat = home.servers[*index].chats.get(channel);
                    let msg = chat.and_then(|chat| chat.messages.iter().find(|msg| msg.id == *id));
                    let files = chat.map_or_else(Vec::new, |chat| own_files(&chat.messages, id));
                    let sender = msg.map_or([0; 32], |msg| msg.person);
                    // A forum post goes together with the poll it carries.
                    let server = &home.servers[*index];
                    if (server.channel(channel)).is_some_and(|c| c.forum)
                        && let Some(chat) = server.chats.get(channel)
                        && let Some(poll) = msg.and_then(|msg| forum::carried_poll(chat, msg))
                    {
                        server.send(Command::Delete(*channel, poll, sender, Vec::new()));
                    }
                    command = Some((*index, Command::Delete(*channel, *id, sender, files)));
                }
                close |= ui::secondary(ui, t!("common.cancel")).clicked();
            });
        }
        Dialog::Unshare(index, channel, shared) => {
            ui.label(t!("files.remove_help", name = shared.file.name));
            ui::footer(ui, |ui| {
                if ui::danger(ui, t!("files.remove")).clicked() {
                    let unshare = Command::Unshare(*channel, shared.file.file.id, shared.person);
                    command = Some((*index, unshare));
                }
                close |= ui::secondary(ui, t!("common.cancel")).clicked();
            });
        }
        Dialog::DeleteDm(peer, id) => {
            ui.label(t!("home.delete_message_help"));
            ui::footer(ui, |ui| {
                if ui::danger(ui, t!("common.delete")).clicked() {
                    delete_dm = Some((*peer, *id));
                }
                close |= ui::secondary(ui, t!("common.cancel")).clicked();
            });
        }
        Dialog::Poke(person, note) => {
            if let Some(text) = pokes::dialog(ui, note, &mut close) {
                poke = Some((*person, text));
            }
        }
        Dialog::Leave(index, error) => {
            ui.label(t!("home.leave_help"));
            error_text(ui, error.as_deref());
            ui::footer(ui, |ui| {
                if ui::danger(ui, t!("common.leave")).clicked() {
                    // Stays open until the server confirms; then the server is removed.
                    *error = None;
                    home.servers[*index].send(Command::Leave);
                }
                close |= ui::secondary(ui, t!("common.cancel")).clicked();
            });
        }
    });
    if let Some((index, choice)) = name_choice {
        let address = home.servers[index].address.clone();
        let mut name = None;
        home.change_profile(|settings, _| {
            name = match choice {
                Some(name) => {
                    settings.name_on(&address, &name);
                    Some(name)
                }
                None => settings.use_default_name(&address),
            };
            Vec::new()
        });
        // Sent even while offline, so the person hears it didn't work.
        command = name.map(|name| (index, Command::SetDisplayName(name)));
    }
    if let Some((index, command)) = command {
        home.servers[index].send(command);
        close = true;
    }
    if let Some((peer, id)) = delete_dm {
        home.delete_dm(peer, id, ctx.input(|input| input.time));
        close = true;
    }
    let now = ctx.input(|input| input.time);
    if let Some((person, text)) = poke {
        home.poke(person, text, now);
        close = true;
    }
    match person_action {
        Some((person, Some(act))) => home.befriend(person, act, now),
        Some((person, None)) => {
            home.open_dm(person, now);
            close = true;
        }
        None => {}
    }
    if close || dismissed {
        home.dialog = None;
    }
    if let Some((index, id)) = delete_channel {
        home.edit_channel(index, id, ChannelEdit::Delete);
    }
}

/// A name field with a confirm button; returns true when confirmed with a non-empty name.
fn text_dialog(ui: &mut Ui, text: &mut String, confirm: &str, close: &mut bool) -> bool {
    let (_, submitted) = name_field(ui, text);
    text_footer(ui, confirm, submitted, close) && !text.trim().is_empty()
}

/// The field, and whether Enter was pressed in it (asked before it takes focus again).
fn name_field(ui: &mut Ui, text: &mut String) -> (Response, bool) {
    let field = ui.add(ui::field(text).char_limit(100).desired_width(f32::INFINITY));
    let entered = submitted(ui, &field);
    ui::focus_when_none(ui, &field);
    (field, entered)
}

fn text_footer(ui: &mut Ui, confirm: &str, mut done: bool, close: &mut bool) -> bool {
    ui::footer(ui, |ui| {
        done |= ui::primary(ui, confirm).clicked();
        *close |= ui::secondary(ui, t!("common.cancel")).clicked();
    });
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn using_other_apps_keeps_this_device_active() {
        let long = AWAY_AFTER + 1.0;
        // No input in the app for long, but some elsewhere a minute ago.
        assert!(!device_idle(false, long, Some(60.0)));
        // No input anywhere for long.
        assert!(device_idle(false, long, Some(long)));
        // The system reading can lag behind input in the app.
        assert!(!device_idle(false, 5.0, Some(long)));
        // Without the system's idle time only the app's input counts, as before.
        assert!(device_idle(false, long, None));
        assert!(!device_idle(false, 60.0, None));
    }

    #[test]
    fn input_anywhere_ends_idle() {
        let long = AWAY_AFTER + 1.0;
        // Back in another app, with the RustTalk window untouched.
        assert!(!device_idle(false, long, Some(3.0)));
        // Input in the app or a call always counts.
        assert!(!device_idle(true, long, Some(long)));
        assert!(!device_idle(true, long, None));
    }

    #[test]
    fn only_its_own_toast_uses_up_the_unsent_notice() {
        let notice = t!("home.unsent_after_close").to_owned();
        // Another toast shows and is drawn: the notice waits.
        let mut toast = Some(("Server offline".to_owned(), 5.0));
        assert!(!unsent_notice(
            &mut toast,
            Some("Server offline".into()),
            1.0
        ));
        assert_eq!(
            toast.as_ref().map(|(text, _)| text.as_str()),
            Some("Server offline")
        );
        // Once it is gone the notice takes its place, and counts only when drawn.
        toast = None;
        assert!(!unsent_notice(&mut toast, None, 6.0));
        assert_eq!(toast.as_ref().map(|(text, _)| text), Some(&notice));
        assert!(unsent_notice(&mut toast, Some(notice), 7.0));
    }

    #[test]
    fn avatar_render_uses_same_picture_at_roster_and_call_sizes() {
        for dark in [true, false] {
            for repaired in [false, true] {
                let picture = Picture::new(
                    [91; 32],
                    vec![(
                        egui::ColorImage::new(
                            [2, 2],
                            vec![Color32::RED, Color32::BLUE, Color32::GREEN, Color32::YELLOW],
                        ),
                        100,
                    )],
                    Default::default(),
                );
                let dir = Directory {
                    members: vec![Member {
                        person: ME,
                        name: Some("Synthetic member".into()),
                        devices: Vec::new(),
                        roles: Vec::new(),
                        avatar: repaired.then_some(picture),
                        presence: Presence::Online,
                        hide_offline: false,
                        bio: None,
                        voice_muted: false,
                    }]
                    .into(),
                    ..Default::default()
                };
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(Vec2::new(360.0, 240.0))
                    .build_ui_state(
                        |ui, dir: &mut Option<Directory>| {
                            let Some(dir) = dir else {
                                return;
                            };
                            ui.label("Member list");
                            face(ui, dir, &ME, "Synthetic member", 32.0, false);
                            ui.label("Call tile");
                            face(ui, dir, &ME, "Synthetic member", 128.0, false);
                        },
                        None,
                    );
                ui::install(&harness.ctx);
                harness.ctx.set_theme(if dark {
                    egui::Theme::Dark
                } else {
                    egui::Theme::Light
                });
                *harness.state_mut() = Some(dir);
                harness.run();
                let images = harness
                    .output()
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::epaint::Shape::Mesh(mesh)
                            if mesh.texture_id != egui::TextureId::default() =>
                        {
                            Some(mesh.texture_id)
                        }
                        egui::epaint::Shape::Rect(rect) => {
                            rect.brush.as_ref().map(|brush| brush.fill_texture_id)
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if repaired {
                    assert_eq!(images.len(), 2);
                    assert_eq!(images[0], images[1]);
                } else {
                    assert!(images.is_empty());
                }
                if let Ok(path) = std::env::var("RUSTTALK_AVATAR_TEST_IMAGES") {
                    let theme = if dark { "dark" } else { "light" };
                    let state = if repaired { "after" } else { "before" };
                    harness
                        .render()
                        .unwrap()
                        .save(format!("{path}/{theme}-{state}.png"))
                        .unwrap();
                }
            }
        }
    }

    #[test]
    fn animated_picture_plays_only_while_its_person_speaks() {
        let frame = |color| (egui::ColorImage::new([2, 2], vec![color; 4]), 100);
        let picture = Picture::new(
            [92; 32],
            vec![
                frame(Color32::RED),
                frame(Color32::BLUE),
                frame(Color32::GREEN),
            ],
            Default::default(),
        );
        let dir = Directory {
            members: vec![Member {
                person: ME,
                name: Some("Synthetic member".into()),
                devices: Vec::new(),
                roles: Vec::new(),
                avatar: Some(picture),
                presence: Presence::Online,
                hide_offline: false,
                bio: None,
                voice_muted: false,
            }]
            .into(),
            ..Default::default()
        };
        let mut h = egui_kittest::Harness::builder()
            .with_size(Vec2::new(200.0, 200.0))
            .build_ui_state(
                move |ui, speaking: &mut bool| {
                    face_speaking(ui, &dir, &ME, "Synthetic member", 64.0, true, *speaking);
                },
                false,
            );
        ui::install(&h.ctx);
        h.run();
        // The pointer stays away, so only speaking can play it. The three frames last
        // 100 ms each and loop on the clock, so each time below lands on a later frame.
        let at = |h: &mut egui_kittest::Harness<bool>, seconds: f64| {
            h.input_mut().time = Some(seconds);
            h.step();
            let texture = (h.output().shapes.iter())
                .find_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Rect(rect) => {
                        rect.brush.as_ref().map(|brush| brush.fill_texture_id)
                    }
                    _ => None,
                })
                .expect("the picture is painted");
            let delay = h.output().viewport_output[&egui::ViewportId::ROOT].repaint_delay;
            (texture, delay)
        };
        let (first, _) = at(&mut h, 0.0);
        for seconds in [0.15, 0.25] {
            assert_eq!(
                at(&mut h, seconds),
                (first, Duration::MAX),
                "silent stays still"
            );
        }
        *h.state_mut() = true;
        let (second, delay) = at(&mut h, 0.45);
        assert_ne!(second, first, "speaking plays the next frame");
        assert!(
            delay <= Duration::from_millis(100),
            "and asks for the one after"
        );
        let (third, _) = at(&mut h, 0.55);
        assert!(third != first && third != second);
        *h.state_mut() = false;
        assert_eq!(
            at(&mut h, 0.75),
            (first, Duration::MAX),
            "back to the first frame"
        );
        // "Animated emoji: On hover" keeps pictures still while people speak too.
        crate::emoji::set_hover_only(&h.ctx, true);
        *h.state_mut() = true;
        assert_eq!(at(&mut h, 0.85), (first, Duration::MAX));
    }

    #[test]
    fn lazy_gif_plays_and_keeps_only_its_first_frame_in_ram() {
        use image::{Delay, Frame, Rgba, RgbaImage, codecs::gif::GifEncoder};
        let mut file = Vec::new();
        let mut encoder = GifEncoder::new(&mut file);
        (encoder.encode_frames((0..3u8).map(|index| {
            let image = RgbaImage::from_pixel(4, 4, Rgba([index * 80, 30, 60, 255]));
            Frame::from_parts(image, 0, 0, Delay::from_numer_denom_ms(100, 1))
        })))
        .unwrap();
        drop(encoder);
        let picture = crate::net::picture(&file).unwrap();
        let shown = picture.clone();
        let mut h = egui_kittest::Harness::builder()
            .with_size(Vec2::new(100.0, 100.0))
            .build_ui(move |ui| {
                let rect = Rect::from_min_size(ui.min_rect().min, Vec2::splat(64.0));
                paint_playing(ui, rect, &shown, 0.0, true);
            });
        let mut textures = std::collections::HashSet::new();
        for step in 0..2000 {
            h.input_mut().time = Some(f64::from(step) * 0.05);
            h.step();
            textures.extend(
                h.output()
                    .shapes
                    .iter()
                    .filter_map(|shape| match &shape.shape {
                        egui::epaint::Shape::Rect(rect) => {
                            rect.brush.as_ref().map(|b| b.fill_texture_id)
                        }
                        _ => None,
                    }),
            );
            if textures.len() == 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(textures.len(), 3, "every frame plays");
        assert_eq!(
            picture.held_bytes(),
            4 * 4 * 4,
            "the other frames live on the GPU only"
        );
    }

    const ME: Id = [1; 32];
    const ANN: Id = [7; 32];
    const PROJECTS: Id = [3; 32];

    mod album;
    mod audit_log;
    mod avatars;
    mod blocking;
    mod call_controls;
    mod camera_pip;
    mod diagram;
    mod dialogs;
    mod emoji;
    mod empty_categories;
    mod file_limits;
    mod full_screen;
    mod gallery;
    mod image_reload;
    mod joining;
    mod long_chat;
    mod member_list;
    mod menus;
    mod message_history;
    mod message_spacing;
    mod outbox;
    mod presence;
    mod rail;
    mod reactions;
    mod reply_jump;
    mod server_emoji;
    mod settings_close;
    mod sticker_picker;
    mod streams;
    mod switcher;
    mod touch;
    mod uploads;
    mod voice_limit;

    /// A server with a text channel this person can write in, showing its files: one
    /// from Ann and one of their own. Also returns the commands the view sends.
    pub(super) fn files_harness(
        dark: bool,
    ) -> (
        egui_kittest::Harness<'static, Option<Home>>,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
    ) {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(Vec2::new(720.0, 420.0))
            .build_ui_state(
                |ui, home: &mut Option<Home>| {
                    if let Some(home) = home {
                        chat_pane(ui, home, 0, PROJECTS);
                    }
                },
                None,
            );
        ui::install(&harness.ctx);
        harness.ctx.set_theme(if dark {
            egui::Theme::Dark
        } else {
            egui::Theme::Light
        });
        let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Desktop").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut home = Home::new(identity, &harness.ctx, data.path().to_owned()).unwrap();
        let (commands, sent) = tokio::sync::mpsc::unbounded_channel();
        let known = KnownServer {
            address: "https://friends.example.org/".parse().unwrap(),
            server_id: [2; 32],
            tls_fingerprint: None,
        };
        let mut server = new_server(known, "Friends", commands);
        server.status = Status::Online;
        server.dir.me = ME;
        let member = |person, name: &str| Member {
            person,
            name: Some(name.into()),
            devices: Vec::new(),
            roles: vec!["member".into()],
            avatar: None,
            presence: Presence::Online,
            hide_offline: false,
            bio: None,
            voice_muted: false,
        };
        server.dir.members = vec![member(ME, "Me"), member(ANN, "Ann")].into();
        server.dir.categories = vec![crate::server::Category {
            id: None,
            name: "Text".into(),
            channels: vec![crate::server::Channel {
                id: PROJECTS,
                name: "projects".into(),
                topic: String::new(),
                voice: false,
                page: false,
                forum: false,
                audio_kbps: 0,
                member_limit: 0,
                focus_until: None,
                flags: vec![("member".into(), 3)],
            }],
        }];
        let shared = |person, name: &str, size: usize| rusttalk_core::client::SharedFile {
            person,
            at: 1_700_000_000,
            file: rusttalk_core::client::prepare_file(name, "application/pdf", &vec![0; size], &[])
                .unwrap()
                .0,
        };
        let mut chat = Chat::default();
        chat.opened = true;
        chat.shared = Some(vec![
            shared(ANN, "Trip plan.pdf", 240_000),
            shared(ME, "Budget.xlsx", 18_000),
        ]);
        server.chats.insert(PROJECTS, chat);
        home.servers.push(server);
        harness
            .ctx
            .data_mut(|d| d.insert_temp(files_id(PROJECTS), true));
        *harness.state_mut() = Some(home);
        harness.run_steps(4);
        (harness, sent)
    }

    #[test]
    fn cached_dm_keeps_the_composer_and_other_replicas_on_refresh() {
        for dark in [true, false] {
            let (mut base, _sent) = files_harness(dark);
            let mut home = base.state_mut().take().unwrap();
            let mut dir = home.servers[0].dir.clone();
            dir.dms.push(crate::server::Dm {
                channel: PROJECTS,
                peer: ANN,
            });
            dir.categories.clear();
            let online = home.servers[0].address.clone();
            let offline: url::Url = "https://offline.example.org/".parse().unwrap();
            let known = KnownServer {
                address: offline.clone(),
                server_id: [4; 32],
                tls_fingerprint: None,
            };
            home.servers.push(new_server(
                known,
                "Offline",
                home.servers[0].commands.clone(),
            ));
            let msg = |n| Msg {
                id: [n; 32],
                person: ANN,
                at: n.into(),
                body: Body::Text(format!("cached {n}")),
                backfilled: false,
                reply_to: None,
                quoted_author: None,
                edit_of: None,
                files: vec![],
                live: None,
                sticker: None,
                replicas: vec![],
                copy_files: vec![],
            };
            for (url, items) in [
                (offline.clone(), vec![msg(1), msg(2)]),
                (online.clone(), vec![msg(2), msg(3)]),
            ] {
                home.apply(
                    &base.ctx,
                    Update::Cached(url, Box::new(dir.clone()), vec![(PROJECTS, items)]),
                    0.0,
                );
            }
            let mut h = egui_kittest::Harness::builder()
                .with_size(Vec2::new(720., 420.))
                .with_context_setup(ui::install)
                .build_ui_state(|ui, home| dm_pane(ui, home, ANN), home);
            h.ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            h.run_steps(2);
            h.ctx.memory_mut(|m| m.request_focus(composer_id(PROJECTS)));
            h.event(egui::Event::Text("still typing".into()));
            h.run_steps(2);
            let ctx = h.ctx.clone();
            let home = h.state_mut();
            let chat = &mut home.threads.get_mut(&ANN).unwrap().chat;
            assert_eq!(chat.draft, "still typing");
            assert_eq!(chat.messages.len(), 3);
            chat.reply_to = Some((ANN, [1; 32]));
            home.apply(
                &ctx,
                Update::PageFailed(online.clone(), PROJECTS, false),
                0.0,
            );
            assert_eq!(home.threads[&ANN].chat.messages.len(), 3);
            home.apply(
                &ctx,
                Update::Messages {
                    server: online,
                    channel: PROJECTS,
                    items: vec![],
                    older: false,
                    page: Some((true, None)),
                },
                0.0,
            );
            let chat = &home.threads[&ANN].chat;
            assert_eq!(chat.messages.len(), 2);
            assert!(
                chat.messages
                    .iter()
                    .all(|m| m.replicas == [offline.clone()])
            );
            assert_eq!(chat.draft, "still typing");
            assert_eq!(chat.reply_to, Some((ANN, [1; 32])));
        }
    }

    #[test]
    fn a_notice_before_first_open_still_loads_channel_history() {
        let (mut h, mut sent) = files_harness(true);
        while sent.try_recv().is_ok() {}
        let ctx = h.ctx.clone();
        let home = h.state_mut().as_mut().unwrap();
        let server = &mut home.servers[0];
        server.chats.remove(&PROJECTS);
        let other = [42; 32];
        server.chats.insert(other, Chat::default());
        let address = server.address.clone();
        let msg = Msg {
            backfilled: false,
            id: [12; 32],
            person: ANN,
            at: 1,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            body: Body::Notice {
                name: "Ann".into(),
                action: "joined",
                detail: String::new(),
            },
            files: vec![],
            live: None,
            sticker: None,
            replicas: vec![],
            copy_files: vec![],
        };
        home.apply(&ctx, Update::Notice(address, PROJECTS, msg), 0.0);
        assert!(home.servers[0].chats[&other].messages.is_empty());
        assert_eq!(home.servers[0].chats[&PROJECTS].messages.len(), 1);
        h.run_steps(2);
        assert!(matches!(sent.try_recv(), Ok(Command::Open(PROJECTS))));
    }

    type Screen = (Home, crate::update::Updater, crate::keystore::Keystore);

    /// Every main screen fits a phone (390 by 844) and a desktop window (1100 by
    /// 700): nothing reaches past the window's left or right edge. A screen with a
    /// fixed width, or a row that doesn't wrap, fails here (see docs/development.md,
    /// Responsive UI).
    #[test]
    fn main_screens_fit_phone_and_desktop_widths() {
        use egui_kittest::kittest::{NodeT, Queryable};
        type Scene = (&'static str, fn(&mut Home));
        let scenes: [Scene; 9] = [
            ("channel list", |home| home.layout.list = true),
            ("chat", |_| {}),
            ("find servers", |home| {
                home.view = View::Find;
                home.find.error = Some(
                    "Couldn't reach the server directory. Check its address in Settings.".into(),
                );
                home.find.results = (0..3)
                    .map(|at| crate::net::Listed {
                        server: KnownServer {
                            address: format!("https://s{at}.example.org/").parse().unwrap(),
                            server_id: [at; 32],
                            tls_fingerprint: None,
                        },
                        name: "RustTalk Official Server with a long name".into(),
                        description: "A friendly place to talk about anything, with voice channels for games and music.".into(),
                        members: 9,
                        open: true,
                    })
                    .collect();
            }),
            ("join", |home| home.view = View::Join),
            ("settings", |home| home.settings = Some(Tab::Profile)),
            ("appearance", |home| home.settings = Some(Tab::Appearance)),
            ("voice settings", |home| home.settings = Some(Tab::Voice)),
            ("server roles", |home| {
                crate::admin::open(home, 0, crate::admin::Tab::Roles);
            }),
            ("edit channel", |home| {
                home.edit_channel(0, PROJECTS, ChannelEdit::Rename);
            }),
        ];
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            for (name, set) in scenes {
                let (mut files, _sent) = files_harness(true);
                let mut home = files.state_mut().take().unwrap();
                home.view = View::Server(0);
                home.servers[0].open = Some(PROJECTS);
                home.servers[0].dir.perms.manage_roles = true;
                home.servers[0].dir.perms.manage_channels = true;
                home.layout.list = false;
                // Never a search on the real directory from a test.
                home.find.opened = false;
                let mut prefs = home.notify.prefs().clone();
                prefs.desktop = false;
                home.notify.set_prefs(prefs);
                set(&mut home);
                let data = tempfile::tempdir().unwrap();
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(size)
                    .build_ui_state(
                        |ui, screen: &mut Option<Screen>| {
                            if let Some((home, updates, keystore)) = screen {
                                show(ui, home, updates, keystore);
                            }
                        },
                        None,
                    );
                ui::install(&harness.ctx);
                *harness.state_mut() = Some((
                    home,
                    crate::update::Updater::new(data.path()),
                    crate::keystore::without_keychain(data.path()),
                ));
                harness.run_steps(6);
                if name == "settings" && size.x < ui::PHONE_WIDTH {
                    // Phones list the pages first.
                    harness.get_by_label("My account").click_accesskit();
                    harness.run_steps(6);
                }
                let width = f64::from(size.x);
                let outside: Vec<_> = harness
                    .root()
                    .children_recursive()
                    .filter_map(|node| {
                        let node = node.accesskit_node();
                        let b = node.bounding_box()?;
                        (b.x1 > b.x0 && (b.x0 < -1.0 || b.x1 > width + 1.0))
                            .then(|| format!("{:?} {:?}", node.role(), node.label()))
                    })
                    .collect();
                assert!(
                    outside.is_empty(),
                    "{name} at {size:?} runs off the window: {outside:?}"
                );
            }
        }
    }

    /// Previewing as Member: the staff channel and its category go, the composer
    /// says posting is off, and the bar shows the role. Set
    /// `RUSTTALK_VIEW_AS_SCREENSHOTS` to a folder to save it at phone and desktop size.
    #[test]
    fn view_as_shows_less_and_turns_actions_off() {
        use egui_kittest::kittest::Queryable;
        let save = std::env::var_os("RUSTTALK_VIEW_AS_SCREENSHOTS").map(PathBuf::from);
        let role = |id: &str, name: &str, rank, color| crate::server::Role {
            id: id.into(),
            name: name.into(),
            name_known: true,
            rank,
            permissions: Vec::new(),
            custom: false,
            color,
            separate: false,
        };
        let channel = |id, name: &str, voice, readers: &[&str]| crate::server::Channel {
            id,
            name: name.into(),
            topic: String::new(),
            voice,
            page: false,
            forum: false,
            audio_kbps: 64,
            member_limit: 0,
            focus_until: None,
            flags: readers.iter().map(|r| (r.to_string(), 15)).collect(),
        };
        for size in [Vec2::new(390.0, 844.0), Vec2::new(1100.0, 700.0)] {
            for dark in [true, false] {
                let (mut files, _sent) = files_harness(dark);
                let mut home = files.state_mut().take().unwrap();
                files
                    .ctx
                    .data_mut(|d| d.insert_temp(files_id(PROJECTS), false));
                let mut settings = home.voice.audio.settings();
                let mode = if dark {
                    ui::Mode::Dark
                } else {
                    ui::Mode::Light
                };
                settings.set_appearance(ui::Appearance {
                    mode,
                    ..Default::default()
                });
                home.voice.audio.set_settings(settings);
                let server = &mut home.servers[0];
                server.dir.default_role = "member".into();
                server.dir.roles = vec![
                    role(
                        "moderator",
                        "Moderator",
                        50,
                        Some(egui::Color32::from_rgb(0x3b, 0xa5, 0x5d)),
                    ),
                    role(
                        "helpers",
                        "Helpers",
                        30,
                        Some(egui::Color32::from_rgb(0xe6, 0x7e, 0x22)),
                    ),
                    role("member", "Member", 0, None),
                ];
                server.dir.members.edit()[0].roles = vec!["moderator".into(), "member".into()];
                server.dir.perms.manage_channels = true;
                server.dir.perms.rank = 50;
                server.dir.categories = vec![
                    crate::server::Category {
                        id: Some([20; 32]),
                        name: "Staff room".into(),
                        channels: vec![channel([21; 32], "mod-chat", false, &["moderator"])],
                    },
                    crate::server::Category {
                        id: Some([22; 32]),
                        name: "Community".into(),
                        channels: vec![
                            channel(PROJECTS, "projects", false, &["member", "moderator"]),
                            channel([23; 32], "Lounge", true, &["member", "moderator"]),
                        ],
                    },
                ];
                server.dir.categories[1].channels[0].flags =
                    vec![("member".into(), 1), ("moderator".into(), 3)];
                view_as::show_as(server, vec!["member".into()]);
                assert!(!server.dir.perms.manage_channels && server.dir.perms.preview);
                home.view = View::Server(0);
                home.servers[0].open = Some(PROJECTS);
                home.layout.list = size.x < ui::PHONE_WIDTH;
                let data = tempfile::tempdir().unwrap();
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(size)
                    .build_ui_state(
                        |ui, screen: &mut Option<Screen>| {
                            if let Some((home, updates, keystore)) = screen {
                                show(ui, home, updates, keystore);
                            }
                        },
                        None,
                    );
                ui::install(&harness.ctx);
                *harness.state_mut() = Some((
                    home,
                    crate::update::Updater::new(data.path()),
                    crate::keystore::without_keychain(data.path()),
                ));
                harness.run_steps(8);
                assert!(harness.query_by_label_contains("mod-chat").is_none());
                assert!(harness.query_by_label_contains("Staff room").is_none());
                assert!(harness.query_by_label_contains("Lounge").is_some());
                assert!(harness.query_by_label(t!("view_as.exit")).is_some());
                if let Some(out) = &save {
                    let theme = if dark { "dark" } else { "light" };
                    let name = format!("view-as-{}x{}-{theme}", size.x, size.y);
                    let image = harness.render().expect("render");
                    image.save(out.join(format!("{name}.png"))).unwrap();
                    if size.x < ui::PHONE_WIDTH {
                        // Then the chat, with its composer turned off.
                        harness.state_mut().as_mut().unwrap().0.layout.list = false;
                        harness.run_steps(8);
                        let image = harness.render().expect("render");
                        image.save(out.join(format!("{name}-chat.png"))).unwrap();
                    }
                }
                if size.x >= ui::PHONE_WIDTH {
                    assert!(
                        harness
                            .query_by_label_contains(t!("view_as.read_only"))
                            .is_some()
                    );
                }
                harness.get_by_label(t!("view_as.exit")).click_accesskit();
                harness.run_steps(4);
                let home = &harness.state().as_ref().unwrap().0;
                assert!(home.servers[0].view_as.is_none());
                assert!(home.servers[0].dir.perms.manage_channels);
            }
        }
    }

    /// On a phone the channel list follows a finger in from the left edge, opens
    /// past half way, and closes with a drag to the left. Set
    /// `RUSTTALK_DRAWER_SCREENSHOTS` to a folder to save it mid-drag and open.
    #[test]
    fn the_phone_channel_list_swipes_in_from_the_edge() {
        use egui::{Event, Modifiers, PointerButton, Pos2, pos2};
        fn pointer(h: &mut egui_kittest::Harness<'_, Option<Screen>>, pos: Pos2, down: bool) {
            h.input_mut().events.push(Event::PointerMoved(pos));
            h.input_mut().events.push(Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: down,
                modifiers: Modifiers::NONE,
            });
            h.step();
        }
        fn slide(h: &mut egui_kittest::Harness<'_, Option<Screen>>, from: Pos2, to: Pos2) {
            for i in 1..=12 {
                let at = from + (to - from) * (i as f32 / 12.0);
                h.input_mut().events.push(Event::PointerMoved(at));
                h.step();
            }
            // Hold still: no fling on release.
            h.run_steps(10);
        }
        let list = |h: &egui_kittest::Harness<'_, Option<Screen>>| {
            h.state().as_ref().unwrap().0.layout.list
        };
        for dark in [true, false] {
            let (mut files, _sent) = files_harness(dark);
            let mut home = files.state_mut().take().unwrap();
            home.view = View::Server(0);
            home.servers[0].open = Some(PROJECTS);
            home.layout.list = false;
            let mut prefs = home.notify.prefs().clone();
            prefs.desktop = false;
            home.notify.set_prefs(prefs);
            let data = tempfile::tempdir().unwrap();
            let mut h = egui_kittest::Harness::builder()
                .with_size(Vec2::new(390.0, 844.0))
                .build_ui_state(
                    |ui, screen: &mut Option<Screen>| {
                        if let Some((home, updates, keystore)) = screen {
                            show(ui, home, updates, keystore);
                        }
                    },
                    None,
                );
            ui::install(&h.ctx);
            h.ctx.set_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
            *h.state_mut() = Some((
                home,
                crate::update::Updater::new(data.path()),
                crate::keystore::without_keychain(data.path()),
            ));
            h.run_steps(6);
            let capture = |h: &mut egui_kittest::Harness<'_, Option<Screen>>, phase: &str| {
                if let Some(dir) = std::env::var_os("RUSTTALK_DRAWER_SCREENSHOTS") {
                    let dir = std::path::PathBuf::from(dir);
                    std::fs::create_dir_all(&dir).unwrap();
                    let theme = if dark { "dark" } else { "light" };
                    h.render()
                        .unwrap()
                        .save(dir.join(format!("drawer-{phase}-390-{theme}.png")))
                        .unwrap();
                }
            };
            // A drag from the middle of the chat leaves the list shut.
            pointer(&mut h, pos2(150.0, 400.0), true);
            slide(&mut h, pos2(150.0, 400.0), pos2(380.0, 400.0));
            pointer(&mut h, pos2(380.0, 400.0), false);
            h.run();
            assert!(!list(&h));
            // From the edge it follows the finger, and opens past half way.
            pointer(&mut h, pos2(6.0, 400.0), true);
            slide(&mut h, pos2(6.0, 400.0), pos2(170.0, 400.0));
            capture(&mut h, "mid-drag");
            slide(&mut h, pos2(170.0, 400.0), pos2(260.0, 400.0));
            pointer(&mut h, pos2(260.0, 400.0), false);
            h.run();
            assert!(list(&h));
            capture(&mut h, "open");
            // A drag to the left across the list closes it.
            pointer(&mut h, pos2(300.0, 500.0), true);
            slide(&mut h, pos2(300.0, 500.0), pos2(60.0, 500.0));
            pointer(&mut h, pos2(60.0, 500.0), false);
            h.run();
            assert!(!list(&h));
        }
    }

    /// Back on a phone hides the chat at once: a message handled before the next
    /// frame draws the list stays unread.
    #[test]
    fn a_message_right_after_back_on_a_phone_stays_unread() {
        use egui_kittest::kittest::Queryable;
        let (mut files, _sent) = files_harness(true);
        let mut home = files.state_mut().take().unwrap();
        home.view = View::Server(0);
        home.servers[0].open = Some(PROJECTS);
        home.layout.list = false;
        // Never a real desktop notification from a test.
        let mut prefs = home.notify.prefs().clone();
        prefs.desktop = false;
        home.notify.set_prefs(prefs);
        let data = tempfile::tempdir().unwrap();
        let mut phone = egui_kittest::Harness::builder()
            .with_size(Vec2::new(390.0, 844.0))
            .build_ui_state(
                |ui, screen: &mut Option<Screen>| {
                    if let Some((home, updates, keystore)) = screen {
                        show(ui, home, updates, keystore);
                    }
                },
                None,
            );
        ui::install(&phone.ctx);
        *phone.state_mut() = Some((
            home,
            crate::update::Updater::new(data.path()),
            crate::keystore::without_keychain(data.path()),
        ));
        phone.run_steps(4);
        phone.get_by_label("Back").click();
        phone.run_steps(1);
        let ctx = phone.ctx.clone();
        let (home, ..) = phone.state_mut().as_mut().unwrap();
        let address = home.servers[0].address.clone();
        let msg = Msg {
            backfilled: false,
            id: [9; 32],
            person: ANN,
            at: notify::unix_now() + 1,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            body: Body::Text("Are you there?".into()),
            files: Vec::new(),
            live: None,
            sticker: None,
            replicas: Vec::new(),
            copy_files: Vec::new(),
        };
        home.arrived(&ctx, 0, PROJECTS, msg);
        phone.run_steps(2);
        let (home, ..) = phone.state().as_ref().unwrap();
        assert!(home.layout.list);
        assert!(home.notify.unread(&address, &PROJECTS));
    }

    /// Android's Back on a phone closes what is on top, one thing a press: a
    /// settings page goes back to the page list, a dialog closes, the open pane goes
    /// back to the channel list, and at the list nothing is left to close.
    #[test]
    fn back_on_a_phone_closes_the_top_thing_first() {
        use egui_kittest::kittest::Queryable;
        let (mut files, _sent) = files_harness(true);
        let mut home = files.state_mut().take().unwrap();
        home.view = View::Server(0);
        home.servers[0].open = Some(PROJECTS);
        home.servers[0].dir.perms.manage_channels = true;
        home.layout.list = false;
        home.find.opened = false;
        let mut prefs = home.notify.prefs().clone();
        prefs.desktop = false;
        home.notify.set_prefs(prefs);
        let data = tempfile::tempdir().unwrap();
        let mut phone = egui_kittest::Harness::builder()
            .with_size(Vec2::new(390.0, 844.0))
            .build_ui_state(
                |ui, screen: &mut Option<Screen>| {
                    if let Some((home, updates, keystore)) = screen {
                        show(ui, home, updates, keystore);
                        let open = crate::back::frame_end(ui.ctx(), home.can_go_back());
                        ui.data_mut(|data| {
                            data.insert_temp(egui::Id::unique("back-test-open"), open)
                        });
                    }
                },
                None,
            );
        ui::install(&phone.ctx);
        *phone.state_mut() = Some((
            home,
            crate::update::Updater::new(data.path()),
            crate::keystore::without_keychain(data.path()),
        ));
        let back = |phone: &mut egui_kittest::Harness<'_, Option<Screen>>| {
            let ctx = phone.ctx.clone();
            let mut raw = egui::RawInput::default();
            crate::back::press(&mut raw.events);
            let (home, ..) = phone.state_mut().as_mut().unwrap();
            let handled = crate::back::input(&ctx, &mut raw, || home.go_back());
            phone.input_mut().events.extend(raw.events);
            phone.run_steps(4);
            handled.expect("a press")
        };
        let open = |phone: &egui_kittest::Harness<'_, Option<Screen>>| {
            phone
                .ctx
                .data(|data| data.get_temp::<bool>(egui::Id::unique("back-test-open")))
        };
        let home = |phone: &egui_kittest::Harness<'_, Option<Screen>>| {
            let (home, ..) = phone.state().as_ref().unwrap();
            (
                home.settings.is_some(),
                home.dialog.is_some(),
                home.layout.list,
            )
        };
        phone.run_steps(4);
        phone.state_mut().as_mut().unwrap().0.settings = Some(Tab::Profile);
        phone.run_steps(4);
        phone.get_by_label("My account").click_accesskit();
        phone.run_steps(4);
        assert!(back(&mut phone));
        assert_eq!(home(&phone), (true, false, false), "page list");
        assert!(phone.query_by_label("My account").is_some());
        assert!(back(&mut phone));
        assert_eq!(home(&phone), (false, false, false), "settings closed");

        phone
            .state_mut()
            .as_mut()
            .unwrap()
            .0
            .edit_channel(0, PROJECTS, ChannelEdit::Rename);
        phone.run_steps(4);
        assert_eq!(home(&phone), (false, true, false));
        assert_eq!(open(&phone), Some(true));
        assert!(back(&mut phone));
        assert_eq!(home(&phone), (false, false, false), "dialog closed");
        assert!(back(&mut phone));
        assert_eq!(home(&phone), (false, false, true), "channel list");
        assert_eq!(
            open(&phone),
            Some(false),
            "Android handles Back at the root"
        );
        assert!(!back(&mut phone));
        assert_eq!(home(&phone), (false, false, true));
    }

    #[test]
    fn files_list_downloads_for_all_and_removes_only_own_files() {
        use egui_kittest::kittest::Queryable;
        let (harness, mut sent) = files_harness(true);
        assert!(matches!(
            sent.try_recv(),
            Ok(Command::SharedFiles(PROJECTS))
        ));
        assert!(harness.query_by_label("Trip plan.pdf").is_some());
        assert!(harness.query_by_label("Budget.xlsx").is_some());
        assert_eq!(harness.query_all_by_label("Download").count(), 2);
        assert_eq!(harness.query_all_by_label("Remove").count(), 1);
        assert!(harness.query_by_label("Upload").is_some());
    }

    /// Edit channel holds General and Permissions; Save sends only what changed.
    #[test]
    fn edit_channel_saves_permissions_with_the_same_save() {
        use egui_kittest::kittest::Queryable;
        let (mut harness, mut sent) = files_harness(true);
        while sent.try_recv().is_ok() {}
        let mut harness = {
            let home = harness.state_mut().take().unwrap();
            let mut dialog = egui_kittest::Harness::builder()
                .with_size(Vec2::new(620.0, 560.0))
                .build_ui_state(
                    |ui, home: &mut Option<Home>| {
                        if let Some(home) = home {
                            dialogs(ui.ctx(), home);
                        }
                    },
                    None,
                );
            ui::install(&dialog.ctx);
            *dialog.state_mut() = Some(home);
            dialog
        };
        let home = harness.state_mut().as_mut().unwrap();
        home.servers[0].dir.perms.manage_channels = true;
        home.servers[0].dir.roles = vec![crate::server::Role {
            id: "member".into(),
            name: "Member".into(),
            name_known: true,
            rank: 0,
            permissions: Vec::new(),
            custom: false,
            color: None,
            separate: false,
        }];
        home.edit_channel(0, PROJECTS, ChannelEdit::Rename);
        harness.run_steps(4);
        assert!(harness.query_by_label("Delete channel").is_some());
        harness.get_by_label("Permissions").click_accesskit();
        harness.run_steps(4);
        assert!(harness.query_by_label("Delete channel").is_none());
        // Member may read and write; taking Write away waits for Save.
        harness.get_by_label("✏ Write").click_accesskit();
        harness.run_steps(4);
        assert!(sent.try_recv().is_err(), "a permission saved before Save");
        harness.get_by_label("Save").click_accesskit();
        harness.run_steps(4);
        assert!(matches!(
            sent.try_recv(),
            Ok(Command::Admin(Admin::ChannelFlags(PROJECTS, role, 1))) if role == "member"
        ));
        assert!(sent.try_recv().is_err(), "an unchanged name was saved");
        assert!(harness.state().as_ref().unwrap().dialog.is_none());
        // Delete channel on General asks first.
        let home = harness.state_mut().as_mut().unwrap();
        home.edit_channel(0, PROJECTS, ChannelEdit::Rename);
        harness.run_steps(4);
        harness.get_by_label("Delete channel").click_accesskit();
        harness.run_steps(4);
        assert!(matches!(
            harness.state().as_ref().unwrap().dialog,
            Some(Dialog::DeleteItem(0, reorder::Item::Channel(PROJECTS), _))
        ));
    }

    #[test]
    fn a_name_this_device_cannot_read_is_never_overwritten() {
        let ctx = egui::Context::default();
        let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Desktop").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut home = Home::new(identity, &ctx, data.path().to_owned()).unwrap();
        let me = *home.identity.certificate.person_id();
        let (commands, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let known = KnownServer {
            address: "https://friends.example.org/".parse().unwrap(),
            server_id: [2; 32],
            tls_fingerprint: None,
        };
        let mut server = new_server(known, "Friends", commands);
        server.status = Status::Online;
        server.dir.me = me;
        // Their custom name is there, but sealed under an epoch this device lacks.
        server.dir.my_name_hidden = true;
        server.dir.members = vec![Member {
            person: me,
            name: None,
            devices: Vec::new(),
            roles: Vec::new(),
            avatar: None,
            presence: Presence::Online,
            hide_offline: false,
            bio: None,
            voice_muted: false,
        }]
        .into();
        home.servers.push(server);
        let mut settings = home.voice.audio.settings();
        settings.display_name = "Ann".into();
        home.voice.audio.set_settings(settings);
        home.profile_loaded(0);
        assert!(sent.try_recv().is_err(), "the unreadable name was replaced");
        // Without a default, migration doesn't take the server for nameless either.
        let mut settings = home.voice.audio.settings();
        settings.display_name.clear();
        home.migrate_names(&mut settings);
        assert!(settings.display_name.is_empty() && settings.profile_pending.name.is_empty());
        // Once readable and really nameless, it gets the default.
        home.servers[0].dir.my_name_hidden = false;
        home.profile_loaded(0);
        assert!(matches!(sent.try_recv(), Ok(Command::SetDisplayName(name)) if name == "Ann"));
    }

    #[test]
    fn deleting_a_page_version_keeps_pictures_another_version_shows() {
        let picture = |id: u8| Attachment {
            file: rusttalk_core::client::BlobRef::for_tests([id; 32]),
            name: "map.png".into(),
            media_type: "image/png".into(),
            thumbnail: Some(rusttalk_core::client::BlobRef::for_tests([id + 100; 32])),
        };
        let version = |id: u8, files: Vec<Attachment>| Msg {
            backfilled: false,
            id: [id; 32],
            person: [7; 32],
            at: 0,
            reply_to: None,
            quoted_author: None,
            edit_of: None,
            body: Body::Text("Rules".into()),
            files,
            live: None,
            sticker: None,
            replicas: Vec::new(),
            copy_files: Vec::new(),
        };
        // The second version kept picture 1 and dropped picture 2.
        let messages = vec![
            version(1, vec![picture(1), picture(2)]),
            version(2, vec![picture(1)]),
        ];
        assert_eq!(own_files(&messages, &[1; 32]), vec![[2; 32], [102; 32]]);
        assert!(own_files(&messages, &[2; 32]).is_empty());
    }

    #[test]
    fn join_page_takes_a_server_address() {
        let address = |text| server_address(text).map(|url| url.to_string());
        for text in [
            "chat.example.org",
            "https://chat.example.org",
            "https://chat.example.org/",
        ] {
            assert_eq!(address(text).as_deref(), Some("https://chat.example.org/"));
        }
        assert_eq!(
            address("127.0.0.1:8443").as_deref(),
            Some("https://127.0.0.1:8443/")
        );
        for text in [
            "http://chat.example.org",
            "https://chat.example.org/x",
            "a b",
            "",
        ] {
            assert_eq!(address(text), None, "{text}");
        }
    }

    #[test]
    fn directory_defaults_to_official() {
        let mut settings = crate::audio::Settings::default();
        let (url, pin) = directory(&settings).unwrap();
        assert_eq!(url.as_str(), official::DIRECTORY);
        assert!(pin.is_none());

        settings.index_url = "https://index.example/".into();
        assert_eq!(
            directory(&settings).unwrap().0.as_str(),
            "https://index.example/"
        );
    }

    #[test]
    fn animation_frame_follows_the_clock_and_loops() {
        let delays = [100, 50, 50];
        let at = |now| frame_at(delays.iter().copied(), now);
        assert_eq!(at(0), Some((0, 100)));
        assert_eq!(at(120), Some((1, 30)));
        assert_eq!(at(199), Some((2, 1)));
        // Back to the first frame after 200 ms.
        assert_eq!(at(200 * 7 + 10), Some((0, 90)));
        assert_eq!(frame_at([0u32, 0].into_iter(), 5), None);
    }

    #[test]
    fn find_loads_once_per_opening() {
        let mut find = Find::default();
        assert_eq!(find.auto_page(), None);
        find.open();
        assert_eq!(find.auto_page(), Some(1));
        assert_eq!(find.auto_page(), None);
        find.open();
        assert_eq!(find.auto_page(), Some(1));
    }

    #[test]
    fn new_app_button_opens_download_page_on_phones() {
        use egui_kittest::{Harness, kittest::Queryable};
        let save =
            std::env::var_os("RUSTTALK_SAVE_UPDATE_SCREENSHOTS").map(std::path::PathBuf::from);
        for (phone, size, name) in [
            (true, Vec2::new(390.0, 844.0), "phone"),
            (false, Vec2::new(1100.0, 700.0), "desktop"),
        ] {
            for (mode, theme) in [(ui::Mode::Dark, "dark"), (ui::Mode::Light, "light")] {
                let mut harness = Harness::builder().with_size(size).build_ui_state(
                    |ui, (page, urls): &mut (Option<Tab>, Vec<String>)| {
                        ui::install(ui.ctx());
                        ui::apply(
                            ui.ctx(),
                            ui::Appearance {
                                mode,
                                ..Default::default()
                            },
                        );
                        let c = ui::colors(ui);
                        CentralPanel::default()
                            .frame(Frame::new().fill(c.main).inner_margin(Margin::same(16)))
                            .show(ui, |ui| {
                                if compatibility_banner(ui, Compatibility::AppTooOld, phone) {
                                    get_new_app(ui.ctx(), page, phone);
                                }
                            });
                        ui.ctx().output(|output| {
                            urls.extend(output.commands.iter().filter_map(
                                |command| match command {
                                    egui::OutputCommand::OpenUrl(open) => Some(open.url.clone()),
                                    _ => None,
                                },
                            ))
                        });
                    },
                    (None, Vec::new()),
                );
                harness.run_steps(3);
                if let Some(dir) = &save {
                    let image = harness.render().expect("render banner");
                    image
                        .save(dir.join(format!("app-too-old-{name}-{theme}.png")))
                        .expect("save banner screenshot");
                }
                let label = if phone {
                    t!("home.download_new_app")
                } else {
                    t!("home.open_updates")
                };
                harness.get_by_label(label).click();
                harness.run_steps(2);
                let (page, urls) = harness.state();
                if phone {
                    assert_eq!(urls, &[crate::settings::DOWNLOAD_PAGE]);
                    assert!(page.is_none());
                } else {
                    assert!(urls.is_empty());
                    assert!(*page == Some(Tab::Updates));
                }
            }
        }
    }
}
