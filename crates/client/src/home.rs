//! The signed-in window: server rail, channels, chat and members.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use chrono::{DateTime, Local};
use eframe::egui::{
    self, Align, Align2, CentralPanel, CollapsingHeader, Color32, FontId, Frame, Id as UiId, Key,
    KeyboardShortcut, Label, LayerId, Layout, Margin, Modifiers, Order, Panel, Rect, Response,
    RichText, Role, ScrollArea, Sense, Shape, Stroke, StrokeKind, TextEdit, TextureHandle,
    TextureOptions, Ui, UiBuilder, Vec2, WidgetInfo, ecolor::Hsva,
};
use rusttalk_core::{
    auth::{Fingerprint, InviteLink, decode},
    client::{FriendSignal, MAX_TEXT_CHARS, voice::LiveFileEvent},
    identity::{DeviceIdentity, fingerprint},
    index::https_url,
    presence::{Presence, Status as Choice},
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
    net::{
        Admin, AdminUpdate, Admission, Command, Compatibility, Draft, Listed, Net, PAGE_SIZE,
        Preview, Update,
    },
    notify::{self, Arrival, Notify},
    official, reorder,
    screen::Video,
    server::{Body, Chat, Directory, Id, Member, Msg, Picture, Server, Status, member_name},
    ui::{self, Item, Look, Palette, error_text},
};

/// Text fields in settings rows.
const FIELD_WIDTH: f32 = 280.0;
/// Show the character count once a message gets this long.
const COUNT_FROM: usize = 1_800;
/// The buttons in the message bar.
const COMPOSER_BUTTON: f32 = 36.0;
const DAY: u32 = 24 * 60 * 60;
/// Seconds without any input before "Online" turns into "Away" by itself.
const AWAY_AFTER: f64 = 10.0 * 60.0;
const RAIL_WIDTH: f32 = 72.0;
const RAIL_BUTTON: f32 = 48.0;
const SERVER_HEADER: f32 = 44.0;
const MESSAGE_AVATAR: f32 = 38.0;
const STICKER_SIZE: f32 = 160.0;
const TOOLBAR_BUTTON: Vec2 = Vec2::new(30.0, 26.0);

mod friends;
pub mod layout;
mod markdown;
pub mod mentions;
mod pokes;
mod user_bar;

pub struct Home {
    pub identity: Arc<DeviceIdentity>,
    net: Net,
    pub servers: Vec<Server>,
    /// DMs by the other person, merged over every server they were on.
    threads: HashMap<Id, Thread>,
    /// Friends, requests and blocks; synced between this person's devices only.
    friends: Friends,
    friends_tab: friends::Tab,
    /// Pokes received and sent; kept on this device only.
    pokes: crate::pokes::Pokes,
    /// The pop-up for the newest poke.
    popup: Option<pokes::Popup>,
    /// This person's DM messages they deleted, on any device. All of them stay in
    /// `dm_deleted.json` here; the device sync copy holds the newest that fit.
    dm_deleted: dm::Tombstones,
    dm_deleted_path: PathBuf,
    view: View,
    invite: String,
    join: Join,
    find: Find,
    dialog: Option<Dialog>,
    pub toast: Option<(String, f64)>,
    pub settings: Option<Tab>,
    pub voice: Voice,
    pub files: Files,
    emoji_picker: Picker,
    pub confirm_sign_out: bool,
    pub error: Option<String>,
    pub server_settings: Option<admin::Settings>,
    /// The crop step for a picked profile picture or server icon.
    crop: Option<crate::crop::Crop>,
    /// The synced voice and video settings as last published or received.
    synced: BTreeMap<String, serde_json::Value>,
    /// Servers left in this session, so a sync that started before can't bring them back.
    left: HashSet<url::Url>,
    /// This person's status on every server; `None` until a server reports it.
    pub status: Option<Choice>,
    /// "Away" was set by itself after no input, so input sets "Online" again.
    auto_away: bool,
    /// When the person last used the window, in egui time.
    active_at: f64,
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
const AUDIO_KBPS: std::ops::RangeInclusive<u32> = 8..=128;

enum Dialog {
    ServerName(usize, String),
    MyName(usize, String),
    Category(usize, String),
    Channel {
        server: usize,
        name: String,
        voice: bool,
        category: Option<Id>,
        new_category: String,
    },
    RenameChannel {
        server: usize,
        id: Id,
        name: String,
        topic: String,
        topic_changed: bool,
        /// Voice channels only: the bitrate in kbps, and whether it was moved.
        audio: Option<(u32, bool)>,
    },
    RenameCategory(usize, Id, String),
    DeleteItem(usize, reorder::Item, String),
    Invite(Invite),
    Member(usize, Id),
    Delete(usize, Id, Id),
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
    Updates,
    About,
}

impl Home {
    pub fn new(identity: DeviceIdentity, ctx: &egui::Context, data: PathBuf) -> Result<Self> {
        let identity = Arc::new(identity);
        let data_dir = data.clone();
        let audio = Audio::new(&data)?;
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
                new_server(
                    known.address.clone(),
                    t!("home.new_server"),
                    net.start(known, None, None),
                )
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
            friends,
            friends_tab: friends::Tab::default(),
            pokes: crate::pokes::Pokes::open(data_dir.join("pokes.json")),
            popup: None,
            dm_deleted,
            dm_deleted_path,
            invite: String::new(),
            join: Join::Idle(None),
            find: Find {
                opened: browse,
                ..Find::default()
            },
            dialog: None,
            toast: None,
            settings: None,
            voice: Voice::new(audio, video, runtime),
            files: Files::new(&data_dir),
            emoji_picker: Picker::new(&data_dir),
            confirm_sign_out: false,
            error: None,
            server_settings: None,
            crop: None,
            synced,
            left: HashSet::new(),
            status: None,
            auto_away: false,
            active_at: 0.0,
            layout,
            notify: Notify::open(&data_dir, notify::unix_now()),
            focused: true,
            restored: false,
        })
    }

    /// Choose a status on every server.
    fn set_status(&mut self, choice: Choice) {
        self.status = Some(choice);
        for server in &self.servers {
            server.send(Command::SetStatus(choice));
        }
    }

    /// Turn "Online" into "Away" after a while without input, and back once used again.
    fn track_activity(&mut self, ui: &Ui) {
        let (now, active) = ui.input(|input| (input.time, !input.events.is_empty()));
        if active {
            self.active_at = now;
            if self.auto_away {
                self.auto_away = false;
                self.set_status(Choice::Online);
            }
        } else if self.status == Some(Choice::Online) && now - self.active_at > AWAY_AFTER {
            self.auto_away = true;
            self.set_status(Choice::Away);
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
        let mut settings = self.voice.audio.settings();
        settings.hide_offline.left(&server.address);
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

    /// Servers this person is a member of too, where a DM with them can go.
    fn shared(&self, peer: &Id) -> Vec<usize> {
        (0..self.servers.len())
            .filter(|index| self.servers[*index].member(peer).is_some())
            .collect()
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

    /// The chat on screen: server index and channel or DM.
    fn viewing(&self) -> Vec<(usize, Id)> {
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

    /// Show a chat, like after a click on its notification.
    fn open_chat(&mut self, address: &url::Url, channel: Id) {
        let Some(index) = self.index(address) else {
            return;
        };
        if let Some(peer) = self.dm_peer(index, &channel) {
            self.view = View::Direct(Some(peer));
            return;
        }
        let server = &mut self.servers[index];
        if server.channel(&channel).is_some() {
            server.open = Some(channel);
            self.view = View::Server(index);
        }
    }

    /// A new message: mark it unread, count a mention and notify, as the person chose.
    fn arrived(&mut self, ctx: &egui::Context, index: usize, channel: Id, msg: Msg) {
        let me = *self.identity.certificate.person_id();
        if msg.person == me || msg.edit_of.is_some() || matches!(msg.body, Body::Notice { .. }) {
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
        // Friend messages only mean something in a DM.
        if !dm && matches!(msg.body, Body::Friend(..)) {
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
        if notify::should_notify(prefs, rule, arrival, self.focused, viewing) {
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
                Body::Text(text) if !text.trim().is_empty() => message.plain(&server.dir),
                _ if !msg.files.is_empty() || msg.live.is_some() => t!("notify.sent_file").into(),
                _ => t!("notify.new_message").into(),
            };
            let target = (server.address.clone(), channel);
            self.notify.show(ctx, &title, &body, target);
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
                ChannelEdit::Rename => Dialog::RenameChannel {
                    server: index,
                    id,
                    name: channel.name.clone(),
                    topic: channel.topic.clone(),
                    topic_changed: false,
                    audio: channel.voice.then_some((channel.audio_kbps, false)),
                },
                ChannelEdit::Delete => {
                    Dialog::DeleteItem(index, reorder::Item::Channel(id), channel.name.clone())
                }
            });
        }
    }

    fn index(&self, address: &url::Url) -> Option<usize> {
        self.servers
            .iter()
            .position(|server| &server.address == address)
    }

    fn apply(&mut self, ctx: &egui::Context, update: Update, now: f64) {
        match update {
            Update::Preview(result) => {
                self.join = match result {
                    Ok(preview) => Join::Preview {
                        preview,
                        name: String::new(),
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
                            name: String::new(),
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
            Update::Status(address, status) => {
                let Some(index) = self.index(&address) else {
                    return;
                };
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
                    server.order_unavailable = false;
                    // Opens that failed while offline.
                    for (channel, chat) in &server.chats {
                        if chat.loading && chat.messages.is_empty() {
                            let _ = server.commands.send(Command::Open(*channel));
                        }
                    }
                    for thread in self.threads.values() {
                        for channel in thread.unanswered(&address) {
                            server.send(Command::Open(channel));
                        }
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
            Update::ChannelOrderUnavailable(address) => {
                if let Some(index) = self.index(&address) {
                    self.servers[index].order_unavailable = true;
                }
            }
            Update::Directory(address, dir) => {
                if let Some(index) = self.index(&address) {
                    let server = &mut self.servers[index];
                    for member in &dir.members {
                        if let Some(name) = &member.name {
                            server.names.insert(member.person, name.clone());
                        }
                    }
                    // Every server gets the same status; one that differs is told the current one.
                    match (self.status, dir.status) {
                        (None, Some(Choice::Away)) => {
                            // shortcut: a stored "Away" counts as automatic and ends with input,
                            // since whether it was picked isn't stored; keep the pick locally if missed.
                            self.status = Some(Choice::Away);
                            self.auto_away = true;
                        }
                        (None, stored) => self.status = stored,
                        (Some(mine), Some(theirs)) if mine != theirs => {
                            server.send(Command::SetStatus(mine));
                        }
                        _ => {}
                    }
                    if let Some(theirs) = dir.hide_offline {
                        let mut settings = self.voice.audio.settings();
                        if let Some(on) = settings.hide_offline.reported(&address, theirs) {
                            server.send(Command::SetHideOffline(on));
                        }
                        self.voice.audio.set_settings(settings);
                    }
                    server.dir = *dir;
                    self.load_dms(index);
                }
            }
            Update::Left(address) => {
                if let Some(index) = self.index(&address) {
                    self.leave_server(index);
                }
            }
            Update::Presence(address, person, presence) => {
                if let Some(index) = self.index(&address)
                    && let Some(member) =
                        (self.servers[index].dir.members.iter_mut()).find(|m| m.person == person)
                {
                    member.presence = presence;
                }
            }
            Update::SaveProgress(id, done) => self.files.progress(id, done),
            Update::Messages {
                server,
                channel,
                items,
                older,
                at_start,
            } => {
                let Some(index) = self.index(&server) else {
                    return;
                };
                if let Some(peer) = self.dm_peer(index, &channel) {
                    for msg in &items {
                        self.friend_message(peer, msg);
                        // The newest page holds pokes sent while this device was away.
                        if !older {
                            self.poke_arrived(ctx, (index, channel), peer, msg, false);
                        }
                    }
                    let me = *self.identity.certificate.person_id();
                    let thread = self.threads.entry(peer).or_default();
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
                chat.merge(items, older);
                if let Some(at_start) = at_start {
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
            Update::PageFailed(address, channel, missing) => {
                if let Some(index) = self.index(&address)
                    && let Some(peer) = self.dm_peer(index, &channel)
                    && let Some(thread) = self.threads.get_mut(&peer)
                {
                    thread.page_failed(&address, channel, missing);
                }
            }
            Update::Notice(address, person, msg) => {
                let Some(index) = self.index(&address) else {
                    return;
                };
                for chat in self.servers[index].chats.values_mut() {
                    chat.merge(vec![msg.clone()], false);
                }
                if let Some(thread) = self.threads.get_mut(&person) {
                    thread.chat.merge(vec![msg], false);
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
            Update::Admin(address, update) => match &mut self.server_settings {
                Some(settings) if self.servers[settings.server].address == address => {
                    settings.apply(update);
                }
                _ => {
                    if let AdminUpdate::Failed(text) = update {
                        self.toast = Some((text, now + 6.0));
                    }
                }
            },
            Update::Call(address, channel, state) => {
                let Some(call) = &mut self.voice.call else {
                    return;
                };
                if call.server != address || call.channel != channel {
                    return;
                }
                match state {
                    Some(state) => {
                        if state == crate::server::CallState::Reconnecting {
                            self.voice.sounds.reconnecting();
                        }
                        call.state = state;
                    }
                    None => self.voice.end_call(),
                }
            }
            Update::Listening(address, channel) => {
                if let Some(call) = &mut self.voice.call
                    && call.server == address
                    && call.channel == channel
                {
                    call.can_talk = false;
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
            Update::ControlEnded(sid, why) => {
                self.toast = Some((call::remote::ended(self, &sid, why), now + 6.0));
            }
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
            update @ (Update::AppStopped(_) | Update::StreamAudio(_)) => {
                call::shared_sound(&mut self.voice, update);
            }
            Update::Sharing(on) => {
                // A late result after Stop or leaving doesn't turn the chip back on.
                if self.voice.sharing != call::Sharing::Off || !on {
                    self.voice.sharing = if on {
                        call::Sharing::On
                    } else {
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
            Update::Uploading(address, channel, what) => {
                if let Some(index) = self.index(&address) {
                    self.chat(index, channel).uploading = what;
                }
            }
            Update::Image(id, fetch, frames) => self.files.image(ctx, id, fetch, frames),
            Update::Saved(id, saved) => self.files.saved(id, saved),
            Update::Synced(synced) => {
                for known in &synced.servers {
                    if self.index(&known.address).is_none() && !self.left.contains(&known.address) {
                        let commands = self.net.start(known.clone(), None, None);
                        self.servers.push(new_server(
                            known.address.clone(),
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
                let adopted = device_sync::adopt(&audio.settings(), &synced.settings);
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
            Update::Unsent(address, draft) => {
                // A poke's note doesn't belong in the composer, where it would go out as text.
                let Some(index) = self.index(&address).filter(|_| !draft.poke) else {
                    return;
                };
                let me = self.servers[index].dir.me;
                let server = &mut self.servers[index];
                let chat = match draft.peer {
                    Some(peer) => &mut self.threads.entry(peer).or_default().chat,
                    None => server.chats.entry(draft.channel).or_default(),
                };
                // Only when nothing new was typed or picked meanwhile, and no other
                // server got this DM.
                let sent = (chat.messages.iter()).any(|msg| msg.id == draft.id && msg.person == me);
                if chat.draft.is_empty() && chat.staged.is_empty() && !sent {
                    chat.open_draft(&server.dir, &draft.text);
                    chat.reply_to = draft.reply_to;
                    chat.editing = draft.edit_of;
                    files::stage(chat, draft.files);
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

/// The presence colour: green, yellow or grey.
pub fn presence_color(c: &Palette, presence: Presence) -> Color32 {
    match presence {
        Presence::Online => c.success,
        Presence::Away => c.warning,
        Presence::Offline => c.offline,
    }
}

fn panel_frame(fill: Color32) -> Frame {
    Frame::new()
        .fill(fill)
        .inner_margin(Margin::same(ui::SPACE_S as i8))
}

pub fn show(ui: &mut Ui, home: &mut Home, updates: &mut crate::update::Updater) -> Option<Action> {
    let now = ui.input(|input| input.time);
    let focused = ui.input(|input| input.viewport().focused).unwrap_or(true);
    if home.focused && !focused {
        // Stepping away: read marks reach the other devices.
        home.publish_settings();
    }
    home.focused = focused;
    while let Ok(update) = home.net.updates.try_recv() {
        home.apply(ui.ctx(), update, now);
    }
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
    let settings = home.voice.audio.settings();
    ui::apply(ui.ctx(), settings.appearance());
    crate::i18n::set(&settings.language);
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
            let chat = &mut home.threads.entry(peer).or_default().chat;
            if let Some(problem) = files::stage(chat, paths) {
                home.toast = Some((problem, now + 6.0));
            }
            continue;
        }
        let (Ask::Attach(address, _)
        | Ask::Save(address, _)
        | Ask::Live(address, _)
        | Ask::Icon(address)
        | Ask::Emoji(address, ..)
        | Ask::Accept(address, ..)) = &ask
        else {
            continue;
        };
        let Some(index) = home.index(address) else {
            continue;
        };
        let server = &mut home.servers[index];
        match ask {
            Ask::Attach(_, channel) => {
                let chat = server.chats.entry(channel).or_default();
                if let Some(problem) = files::stage(chat, paths) {
                    home.toast = Some((problem, now + 6.0));
                }
            }
            Ask::Save(_, file) => {
                if let Some(path) = paths.into_iter().next() {
                    home.files.save(&server.commands, &file, path);
                }
            }
            Ask::Icon(_) | Ask::Emoji(..) => {
                let Some(path) = paths.into_iter().next() else {
                    continue;
                };
                let target = match ask {
                    Ask::Emoji(address, name, sticker) => Target::Emoji {
                        address,
                        name,
                        sticker,
                    },
                    Ask::Icon(address) => Target::Icon(address),
                    _ => unreachable!("matched above"),
                };
                match Crop::open(target, &path) {
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
    if home.toast.as_ref().is_some_and(|(_, until)| *until < now) {
        home.toast = None;
    }
    let colors = ui::colors(ui);
    // Above everything, full screen too: who controls this screen, and the prompt.
    call::remote::sharer(ui, home);
    if home.voice.stage.full.is_some() {
        CentralPanel::default()
            .frame(panel_frame(colors.main))
            .show(ui, |ui| call::full_view(ui, home));
        call::frame(ui.ctx(), home);
        return None;
    }
    home.layout
        .set_window_width(ui.available_width() - RAIL_WIDTH);
    // One column for rail and channel list, so the bottom panel spans both like Discord.
    let channels = Panel::left("sidebar")
        .resizable(false)
        .exact_size(RAIL_WIDTH + home.layout.channels)
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
                .exact_size(home.layout.channels)
                .frame(panel_frame(colors.side))
                .show(ui, |ui| sidebar(ui, home))
                .response
                .rect
        })
        .inner;
    let mut members = None;
    if let View::Server(index) = home.view
        && home.servers[index].status != Status::Connecting
        && home.servers[index].compatibility != Compatibility::AppTooOld
    {
        // Toggled from the channel header; the panel only slides.
        let mut shown = home.layout.members_shown();
        members = Panel::right("members")
            .resizable(false)
            .exact_size(home.layout.members)
            .frame(panel_frame(colors.side))
            .show_collapsible(ui, &mut shown, |ui| member_list(ui, home, index))
            .map(|panel| panel.response.rect);
    }
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
    // Resize handles go last so they sit above the panels on both sides of each edge.
    let mut dragging = layout::resize_edge(
        ui,
        t!("home.resize_channels"),
        channels,
        true,
        &mut home.layout.channels,
        layout::CHANNELS_WIDTH,
        layout::CHANNELS_RANGE,
    );
    if let Some(members) = members {
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
    pokes::popup(ui.ctx(), home);
    admin::window(ui.ctx(), home);
    files::viewer(ui.ctx(), &mut home.files);
    call::screen_dialog(ui.ctx(), home);
    call::frame(ui.ctx(), home);
    let action = crate::settings::show(ui.ctx(), home, updates);
    updates.dialog(ui.ctx());
    crop_dialog(ui.ctx(), home);
    action
}

fn show_compatibility(ui: &mut Ui, home: &mut Home, index: usize) -> bool {
    let compatibility = home.servers[index].compatibility;
    if compatibility_banner(ui, compatibility) {
        home.settings = Some(Tab::Updates);
    }
    compatibility != Compatibility::AppTooOld
}

fn compatibility_banner(ui: &mut Ui, compatibility: Compatibility) -> bool {
    let text = match compatibility {
        Compatibility::Supported => return false,
        Compatibility::ServerTooOld => {
            t!("home.server_too_old")
        }
        Compatibility::AppTooOld => t!("home.app_too_old"),
    };
    ui::card(ui, |ui| {
        ui.add(Label::new(text).wrap());
        compatibility == Compatibility::AppTooOld
            && ui::primary(ui, t!("home.open_updates")).clicked()
    })
}

fn empty(ui: &mut Ui, title: &str, hint: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.label(ui::heading(title));
        ui.weak(hint);
    });
}

/// A round server button that turns into a rounded square when hovered or open.
fn rail_button(ui: &mut Ui, text: &str, label: &str, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(RAIL_BUTTON), Sense::click());
    let c = ui::colors(ui);
    let accent = ui::accent(ui);
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered() || selected,
        ui::HOVER_SECONDS,
    );
    let radius = RAIL_BUTTON / 2.0 - hover * (RAIL_BUTTON / 2.0 - 16.0);
    let fill = c.main.lerp_to_gamma(accent.base, hover);
    let glyph = c.text.lerp_to_gamma(ui::ON_FILL, hover);
    ui.painter().rect_filled(rect, radius, fill);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::new(17.0, ui::bold_family()),
        glyph,
    );
    ui::focus_ring(ui, &response, radius as u8);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    response
        .on_hover_text(label)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn server_rail(ui: &mut Ui, home: &mut Home) {
    ui.vertical_centered(|ui| {
        ui.add_space(4.0);
        let dms = matches!(home.view, View::Direct(_));
        let button = rail_button(ui, "💬", t!("home.direct_messages"), dms);
        rail_marks(ui, &button, dms, home.dm_marks());
        if button.clicked() {
            home.view = View::Direct(None);
        }
        ui.separator();
        for index in 0..home.servers.len() {
            let marks = home.server_marks(index);
            let server = &home.servers[index];
            let selected = home.view == View::Server(index);
            let button = match &server.dir.icon {
                Some(icon) => {
                    let (rect, button) =
                        ui.allocate_exact_size(Vec2::splat(RAIL_BUTTON), Sense::click());
                    let radius = if selected { 16.0 } else { 24.0 };
                    paint_hovered(ui, rect, icon, radius);
                    let button = button.on_hover_text(server.name());
                    let name = server.name().to_owned();
                    button.widget_info(|| WidgetInfo::labeled(Role::Button, true, &name));
                    button
                }
                None => rail_button(ui, &initials(server.name()), server.name(), selected),
            };
            rail_marks(ui, &button, selected, marks);
            if button.clicked() {
                home.view = View::Server(index);
            }
            ui::context_menu(&button, |m| server_menu(m, home, index));
        }
        let adding = matches!(home.view, View::Join | View::Find);
        let plus = rail_button(ui, "+", t!("home.add_server"), adding);
        ui::menu(&plus, |m| {
            if m.item("🔗", t!("home.join_with_invite")).clicked() {
                home.view = View::Join;
            }
            if m.item("🔍", t!("home.find_servers")).clicked() {
                home.view = View::Find;
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
    let (prefs, now) = (home.notify.prefs(), notify::unix_now());
    let chats =
        (chats.iter()).filter(|(server, channel)| !prefs.effective(server, channel, dm, now).muted);
    let (unread, mentions) = chats.fold((false, 0), |(unread, mentions), (server, channel)| {
        (
            unread || home.notify.unread(server, channel),
            mentions + home.notify.mentions(server, channel),
        )
    });
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
            for server in &home.servers {
                server.send(Command::SetAvatar(bytes.clone()));
            }
        }
        Target::Icon(address) => {
            if let Some(index) = home.index(&address) {
                home.servers[index].send(Command::Admin(Admin::SetIcon(bytes)));
            }
        }
        Target::Emoji {
            address,
            name,
            sticker,
        } => {
            if let Some(index) = home.index(&address) {
                let add = Admin::AddEmoji {
                    name,
                    sticker,
                    bytes,
                };
                home.servers[index].send(Command::Admin(add));
            }
        }
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

/// The texture for a picture's current frame, so animated GIFs play.
pub fn frame(ui: &Ui, picture: &Picture) -> TextureHandle {
    frame_when(ui, picture, true)
}

/// Paint a profile picture or server icon in `rect`: its first frame, and it plays
/// only while the pointer is over it, so lists of them cost nothing at rest.
pub fn paint_hovered(ui: &Ui, rect: Rect, picture: &Picture, radius: f32) {
    let play = ui.rect_contains_pointer(rect);
    egui::Image::new(&frame_when(ui, picture, play))
        .corner_radius(radius)
        .paint_at(ui, rect);
}

/// `frame`, with `play` false for the first frame only (and no repaints).
// shortcut: textures stay in egui's memory until restart; evict them if people change pictures often.
fn frame_when(ui: &Ui, picture: &Picture, play: bool) -> TextureHandle {
    let id = UiId::unique(("picture", picture.key));
    let textures = ui.data(|data| data.get_temp::<Arc<Vec<TextureHandle>>>(id));
    let textures = textures.unwrap_or_else(|| {
        let made: Arc<Vec<_>> = Arc::new(
            (picture.frames.iter())
                .map(|(image, _)| {
                    ui.ctx()
                        .load_texture("picture", image.clone(), TextureOptions::LINEAR)
                })
                .collect(),
        );
        ui.data_mut(|data| data.insert_temp(id, made.clone()));
        made
    });
    if play && textures.len() > 1 {
        let now = (ui.input(|input| input.time) * 1000.0) as u64;
        let delays = picture.frames.iter().map(|(_, ms)| *ms);
        if let Some((at, left)) = frame_at(delays, now) {
            ui.ctx().request_repaint_after(Duration::from_millis(left));
            return textures[at].clone();
        }
    }
    textures[0].clone()
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
    }
    pokes::nav_row(ui, home);
    ui.add_space(ui::SPACE_XS);
    // One row per person, however many servers hold a copy.
    let mut peers: Vec<Id> = Vec::new();
    let known = (home.servers.iter())
        .flat_map(|server| server.dir.dms.iter().map(|dm| dm.peer))
        .chain(home.threads.keys().copied());
    for peer in known {
        if !peers.contains(&peer) {
            peers.push(peer);
        }
    }
    let (shown, requests) = home.friends.split(&peers, home.friends_only());
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
    let shared: Vec<&str> = (home.shared(peer).into_iter())
        .map(|i| home.servers[i].name())
        .collect();
    let caption = if shared.is_empty() {
        t!("home.no_shared_server").to_owned()
    } else {
        shared.join(", ")
    };
    if selected {
        let rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 44.0));
        ui.painter()
            .rect_filled(rect, ui::RADIUS_ROW, ui::colors(ui).selected);
    }
    let row = ui::row_button(ui, 44.0, &name, |ui| {
        face(ui, dir, peer, &name, 30.0, true);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add(Label::new(ui::bold(&name)).truncate());
            ui.add(Label::new(ui::caption(&caption)).truncate());
        });
    });
    if !request {
        let replicas = home.replicas(peer);
        let chats: Vec<_> = (replicas.iter())
            .map(|(index, channel)| (&home.servers[*index].address, *channel))
            .collect();
        chat_marks(ui, home, &chats, true, row.rect);
    }
    if row.clicked() {
        home.view = View::Direct(Some(*peer));
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
    let mut configure = None;
    let mut moved = None;
    let mut channel_edit = None;
    let mut category_dialog = None;
    let mut rule_change = None;
    let categories = &server.dir.categories;
    let can_manage = perms.manage_channels && online;
    let can_move = can_manage && !server.order_unavailable && reorder::order(categories).is_some();
    let area = ScrollArea::vertical().animated(false);
    area.show_viewport(ui, |ui, viewport| {
        ui.spacing_mut().item_spacing.y = 2.0;
        let list_id = UiId::unique(("channel order", &server.address));
        let mut list = reorder::List::begin(ui, list_id, categories, can_move);
        for category in categories {
            let block = category.id.unwrap_or_default();
            if let Some(id) = category.id.filter(|_| !category.name.is_empty()) {
                let item = reorder::Item::Category(id);
                let header = list.row(ui, item, block, |ui| {
                    ui.add_space(ui::SPACE_S);
                    ui.horizontal(|ui| {
                        ui.add_space(ui::SPACE_XS);
                        let label = ui::caption_label(ui, &category.name);
                        if perms.manage_channels {
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
                    let header = if can_move {
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
                    let icon = if channel.voice { "🔊" } else { "#" };
                    let label = format!("{icon}  {}", channel.name);
                    let row = ui
                        .add_enabled_ui(online || !channel.voice, |ui| {
                            ui::nav_row(ui, icon, &channel.name, selected, Role::Button, &label)
                        })
                        .inner;
                    let chats = [(&server.address, channel.id)];
                    chat_marks(ui, home, &chats, false, row.rect);
                    if channel.voice {
                        call::room_list(ui, home, server, &channel.id);
                    }
                    row
                });
                if button.clicked() {
                    if channel.voice {
                        talk = Some(channel.id);
                    } else {
                        pick = Some(channel.id);
                    }
                }
                ui::context_menu(&button, |m| {
                    let rule = home.notify.prefs().channel(&channel.id);
                    if let Some(rule) = notify::rule_menu(m, &rule, true, notify::unix_now()) {
                        rule_change = Some((channel.id, rule));
                    }
                    if perms.manage_channels {
                        m.separator();
                        if m.item("⚙", t!("home.channel_settings")).clicked() {
                            configure = Some(channel.id);
                        }
                        if can_manage {
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
                    }
                });
            }
        }
        moved = moved.or(list.end(ui, categories, viewport));
    });
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
    if let Some(channel) = configure {
        admin::open(home, index, admin::Tab::Channel(channel));
    }
    if let Some((id, edit)) = channel_edit {
        home.edit_channel(index, id, edit);
    }
    if category_dialog.is_some() {
        home.dialog = category_dialog;
    }
    if let Some(channel) = pick {
        home.servers[index].open = Some(channel);
    }
    if let Some(channel) = talk {
        call::join(home, index, channel);
    }
    if let Some(category) = add {
        home.dialog = Some(new_channel(&home.servers[index], index, category));
    }
}

/// The server header menu, with only what this person may do.
fn server_menu(m: &mut ui::Menu, home: &mut Home, index: usize) {
    let perms = home.servers[index].dir.perms;
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
        voice: false,
        category: category.or_else(|| default_category(server, false)),
        new_category: category_name(false).into(),
    }
}

/// A category that already holds this kind of channel, else an empty one;
/// `None` makes a new category named after the kind.
fn default_category(server: &Server, voice: bool) -> Option<Id> {
    let categories = || server.dir.categories.iter().filter(|c| c.id.is_some());
    categories()
        .find(|c| c.channels.iter().any(|channel| channel.voice == voice))
        .or_else(|| categories().find(|c| c.channels.is_empty()))
        .and_then(|c| c.id)
}

fn category_name(voice: bool) -> &'static str {
    if voice {
        t!("home.voice_channels")
    } else {
        t!("home.text_channels")
    }
}

pub fn avatar(ui: &mut Ui, person: &Id, name: &str, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    paint_avatar(ui, rect, person, name);
    response
}

fn paint_avatar(ui: &Ui, rect: Rect, person: &Id, name: &str) {
    let size = rect.width();
    let color: Color32 = Hsva::new(f32::from(person[0]) / 255.0, 0.45, 0.6, 1.0).into();
    ui.painter().circle_filled(rect.center(), size / 2.0, color);
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
            paint_hovered(ui, args.rect, &picture, size / 2.0);
        });
    }
    let person = member.person;
    let name = name.to_owned();
    egui::Atom::paint(Vec2::splat(size), move |ui, args| {
        paint_avatar(ui, args.rect, &person, &name);
    })
}

/// A member's picture (or initials) and, with `dot`, their presence: green, yellow or grey.
pub fn face(
    ui: &mut Ui,
    dir: &Directory,
    person: &Id,
    name: &str,
    size: f32,
    dot: bool,
) -> Response {
    let member = dir.members.iter().find(|m| m.person == *person);
    member_face(ui, member, person, name, size, dot)
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
    let response = match member.and_then(|m| m.avatar.as_ref()) {
        Some(picture) => {
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
            paint_hovered(ui, rect, picture, size / 2.0);
            response
        }
        None => avatar(ui, person, name, size),
    };
    if dot {
        let c = ui::colors(ui);
        let color = presence_color(c, member.map_or(Presence::Offline, |m| m.presence));
        let radius = (size * 0.16).max(4.0);
        ui::presence_dot(ui, response.rect, radius, color, c.side);
    }
    response
}

fn member_list(ui: &mut Ui, home: &mut Home, index: usize) {
    let server = &home.servers[index];
    // You always see yourself, also while invisible.
    let listed: Vec<&Member> = (server.dir.members.iter())
        .filter(|m| m.listed() || m.person == server.dir.me)
        .collect();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(ui::SPACE_XS);
        ui::caption_label(ui, &t!("home.members_count", count = listed.len()));
    });
    let perms = server.dir.perms;
    let mut action = None;
    let (mut befriend, mut poke) = (None, None);
    let (mut manage, mut moderate) = (None, None);
    // Roles shown separately get their own heading, highest first; everyone else after.
    let dir = &server.dir;
    let mut groups: Vec<(Option<&str>, Vec<&Member>)> = (dir.roles.iter())
        .filter(|role| role.separate)
        .map(|role| (Some(role.id.as_str()), Vec::new()))
        .collect();
    groups.push((None, Vec::new()));
    for member in listed {
        let heading = dir.heading_role(&member.roles).map(|role| role.id.as_str());
        if let Some((_, list)) = groups.iter_mut().find(|(id, _)| *id == heading) {
            list.push(member);
        }
    }
    let headings = groups.len() > 1;
    ScrollArea::vertical().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for (id, list) in groups.iter().filter(|(_, list)| !list.is_empty()) {
            if headings {
                let caption = id
                    .and_then(|id| dir.roles.iter().find(|role| role.id == id))
                    .map_or_else(
                        || t!("home.members_count", count = list.len()),
                        |role| t!("home.role_count", name = role.name, count = list.len()),
                    );
                ui.horizontal(|ui| {
                    ui.add_space(ui::SPACE_XS);
                    ui::section(ui, &caption);
                });
            }
            for member in list.iter().copied() {
                let name = member.display_name();
                let me = member.person == server.dir.me;
                let label = if me {
                    t!("home.name_you", name = name)
                } else {
                    name.clone()
                };
                let row = ui::row_button(ui, 40.0, &label, |ui| {
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
                ui::menu(&row, |m| {
                    if !me && m.item("💬", t!("home.message")).clicked() {
                        action = Some(Command::OpenDm(member.person));
                    }
                    let blocked = home.friends.relation(&member.person) == Relation::Blocked;
                    if !me && !blocked && m.item("👉", t!("pokes.poke")).clicked() {
                        poke = Some(member.person);
                    }
                    if m.item("ℹ", t!("common.details")).clicked() {
                        home.dialog = Some(Dialog::Member(index, member.person));
                    }
                    if !me {
                        let relation = home.friends.relation(&member.person);
                        if let Some(act) = friends::menu_items(m, relation) {
                            befriend = Some((member.person, act));
                        }
                    }
                    if !me && home.voice.call.is_some() {
                        m.separator();
                        call::stream_menu(m, &home.voice.audio, &(member.person, None));
                    }
                    if !me && (perms.kick || perms.ban || perms.manage_roles) {
                        m.separator();
                        manage = Some(member.person);
                        admin::member_menu(m, &mut moderate, perms);
                    }
                });
            }
        }
    });
    if let Some(command) = action {
        home.servers[index].send(command);
    }
    if let Some(person) = poke {
        home.dialog = Some(Dialog::Poke(person, String::new()));
    }
    if let Some((person, act)) = befriend {
        home.befriend(person, act, ui.input(|input| input.time));
    }
    if let (Some(person), Some(choice)) = (manage, moderate) {
        admin::moderate(home, index, person, choice);
    }
}

fn join(ui: &mut Ui, home: &mut Home) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.2);
        ui.set_max_width(440.0);
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
                if compatibility_banner(ui, Compatibility::AppTooOld) {
                    home.settings = Some(Tab::Updates);
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
                        .push(new_server(address.clone(), label, commands));
                    if !member {
                        let mut settings = home.voice.audio.settings();
                        settings.hide_offline.joined(&address);
                        home.voice.audio.set_settings(settings);
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
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.label(ui::bold(&listed.name));
                    ui.label(ui::caption(host(&listed.server.address)));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
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
                });
            });
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

pub fn privacy_tab(ui: &mut Ui, home: &mut Home) {
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
        ui.set_max_width(640.0);
        ui.label(ui::heading(t!("home.find_servers")));
        if ui.link(t!("home.join_with_invite")).clicked() {
            home.view = View::Join;
        }
        let settings = home.voice.audio.settings();
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
                        ui.horizontal(|ui| {
                            ui.label(ui::title(&listed.name));
                            if listed.open {
                                ui::badge(ui, t!("home.open_joining"), ui::accent(ui).base);
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
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
                            });
                        });
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

fn new_server(
    address: url::Url,
    label: &str,
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
) -> Server {
    Server {
        address,
        label: label.into(),
        status: Status::Connecting,
        compatibility: Compatibility::default(),
        order_unavailable: false,
        dir: Directory::default(),
        chats: HashMap::new(),
        open: None,
        voice: HashMap::new(),
        names: HashMap::new(),
        commands,
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
        "added a new device" => t!("notice.added_device", name = name),
        "removed a device" => t!("notice.removed_device", name = name),
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

/// A voice channel: the call on top and its chat below. Issue #63 replaces this
/// fixed split with a draggable one and layout choices; only this function changes.
fn voice_pane(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    let title = home.servers[index]
        .chat_title(&channel)
        .map(|(name, _)| name)
        .unwrap_or_default();
    pane_title(
        ui,
        "room-title",
        &format!("🔊 {title}"),
        Some(&mut home.layout),
        true,
    );
    // Problems from the call and the voice panel show here; the chat has its own line.
    toast_line(ui, &mut home.toast);
    ui.add_space(ui::SPACE_S);
    let area = ui.available_rect_before_wrap();
    let (stage, divider, chat) = layout::split(area, &home.layout.split);
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

/// Which chat a pane shows.
#[derive(Clone, Copy)]
enum At {
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
    let mut chat = server.chats.remove(&channel).unwrap_or_else(|| {
        // First look at this channel: load it, then follow live updates.
        server.send(Command::Open(channel));
        let mut chat = Chat::default();
        chat.loading = true;
        chat
    });
    chat_view(ui, home, At::Channel(index, channel), &mut chat);
    home.servers[index].chats.insert(channel, chat);
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
    let mut chat = std::mem::take(&mut home.threads.entry(peer).or_default().chat);
    let act = chat_view(ui, home, At::Dm(peer), &mut chat);
    let thread = home.threads.entry(peer).or_default();
    thread.chat = chat;
    thread.refresh();
    if let Some(act) = act {
        home.befriend(peer, act, ui.input(|input| input.time));
    }
}

/// Shows a chat. In a DM, also returns what the person picked on the friend card.
fn chat_view(ui: &mut Ui, home: &mut Home, at: At, chat: &mut Chat) -> Option<Act> {
    let (title, dm, in_voice) = match at {
        At::Channel(index, channel) => {
            let server = &home.servers[index];
            let (title, dm) = server.chat_title(&channel).unwrap_or_default();
            (title, dm, server.is_voice(&channel))
        }
        At::Dm(peer) => (home.peer_name(&peer), true, false),
    };
    let name = title.clone();
    // A voice channel's chat sits under its call, which already shows the title.
    let title = if dm {
        format!("@ {title}")
    } else if in_voice {
        format!("🔊 {title}")
    } else {
        format!("# {title}")
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
            })
            .collect(),
        ..Directory::default()
    };
    let Home {
        servers,
        threads,
        toast,
        dialog,
        files,
        emoji_picker,
        voice,
        layout,
        ..
    } = home;
    let dir = named.map_or(&fallback, |index| &servers[index].dir);
    let online = !read_only && targets.iter().any(|i| servers[*i].status == Status::Online);
    let now = ui.input(|input| input.time);
    if !in_voice {
        pane_title(ui, "chat-title", &title, in_server.then_some(layout), false);
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
        && let Some(problem) = files::stage(chat, dropped)
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
        // It floats over the chat: no line above, room around it.
        .frame(Frame::new().inner_margin(Margin {
            left: 8,
            right: 8,
            top: 4,
            bottom: 8,
        }))
        .show_separator_line(false)
        .show(ui, |ui| {
            // The call above already shows problems in a voice channel.
            if !in_voice {
                toast_line(ui, toast);
            }
            if read_only {
                ui.weak(t!("home.dm_read_only", name = name));
                return None;
            }
            let hint = if dm {
                format!("@{name}")
            } else {
                format!("#{name}")
            };
            let (command, attach) = composer(ui, chat, dir, online, key, &hint, emoji_picker);
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
        drop_zone(ui, &title);
    }
    // The server a message's files and live offer come from.
    let holder = |msg: &Msg| {
        let copy =
            (msg.replicas.first()).and_then(|url| servers.iter().find(|s| s.address == *url));
        copy.or(named.map(|index| &servers[index]))
    };
    let mut area = ScrollArea::vertical()
        .id_salt(("chat", key))
        .stick_to_bottom(true)
        .auto_shrink(false);
    if let Some(offset) = chat.scroll_to.take() {
        area = area.vertical_scroll_offset(offset);
    }
    let mut pick = None;
    let mut live = None;
    let output = area.show(ui, |ui| {
        ui.set_width(ui.available_width());
        if chat.at_start {
            ui.add_space(ui::SPACE_M);
            ui.label(ui::heading(t!("home.chat_start", channel = title)));
            ui.add_space(ui::SPACE_M);
        } else if chat.loading {
            ui.vertical_centered(|ui| ui.spinner());
        }
        let mut last: Option<&Msg> = None;
        for msg in &chat.messages {
            if matches!(msg.body, Body::Friend(signal, _) if !friends::shown(signal))
                || matches!(msg.body, Body::Poke(_))
            {
                continue;
            }
            let start = !last.is_some_and(|prev| {
                prev.person == msg.person
                    && !matches!(prev.body, Body::Notice { .. } | Body::Friend(..))
                    && msg.at - prev.at < 300
                    && msg.reply_to.is_none()
            });
            let shown = message(ui, chat, msg, start, dir, key, |ui| {
                if let Some(server) = holder(msg) {
                    files::attachments(ui, files, &server.address, &server.commands, &msg.files);
                }
                // The server isn't borrowed whole here, so look the offer up by its parts.
                if msg.live.is_some() {
                    let action = call::chat_live(ui, &voice.live_files, dir, msg);
                    live = action
                        .map(|action| (action, msg.replicas.first().cloned()))
                        .or(live.take());
                }
            });
            if let Some(chosen) = shown {
                pick = Some(chosen);
            }
            last = Some(msg);
        }
    });
    let content = output.content_size.y;
    // Only page once the layout has settled; the first frame always starts at the top.
    let settled = chat.content_height == content;
    if let Some(before) = chat.keep_scroll {
        if content > before {
            chat.keep_scroll = None;
            chat.scroll_to = Some(output.state.clamped_offset().y + content - before);
            ui.ctx().request_repaint();
        } else if settled {
            // The page added nothing new (a DM copy of what was loaded): page on.
            chat.keep_scroll = None;
        }
    }
    chat.content_height = content;
    // A DM asks each copy on its own, so one slow server doesn't hold up the others.
    if settled
        && output.state.clamped_offset().y < 4.0
        && (!chat.loading || matches!(at, At::Dm(_)))
        && !chat.at_start
        && chat.keep_scroll.is_none()
    {
        match at {
            At::Channel(index, channel) => {
                let oldest = chat
                    .messages
                    .iter()
                    .find(|msg| !matches!(msg.body, Body::Notice { .. }));
                if let Some(oldest) = oldest {
                    chat.loading = true;
                    servers[index].send(Command::Older(channel, oldest.id));
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
    // A live file's offer lives on one server: in a DM, the one its message came from.
    let live_server = |url: Option<url::Url>| match at {
        At::Channel(index, _) => Some(index),
        At::Dm(_) => url.and_then(|url| servers.iter().position(|s| s.address == url)),
    };
    match live {
        Some((LiveAction::Save(id, name), url)) => {
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
            if let Some(index) = live_server(url) {
                let address = servers[index].address.clone();
                files.ask(ui.ctx(), Ask::Accept(address, id, name));
            }
        }
        Some((LiveAction::Remove(id, stop), url)) => {
            if stop && let Some(index) = live_server(url) {
                servers[index].send(Command::CancelFile(id));
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
        _ => {}
    }
    match pick {
        Some(Pick::Reply(person, id)) => {
            chat.reply_to = Some((person, id));
            if chat.editing.take().is_some() {
                chat.clear_draft();
            }
            ui.memory_mut(|memory| memory.request_focus(composer_id(key)));
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
        None => {}
    }
    act
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
    if !sent {
        chat.draft = draft.text;
        chat.reply_to = draft.reply_to;
        chat.editing = draft.edit_of;
        *toast = Some((t!("home.waiting_try_again").into(), now + 6.0));
    }
}

/// What files dragged over the chat would do: the chat dimmed behind an accent
/// frame that says where they go. On top, because the messages are drawn after it.
fn drop_zone(ui: &Ui, title: &str) {
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
        t!("home.drop_hint", max = files::MAX_FILES),
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
    split: bool,
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
                ui.label(ui::title(title));
                if let Some(layout) = members {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        layout::members_button(ui, layout);
                        if split {
                            ui.add_space(ui::SPACE_S);
                            layout::split_buttons(ui, &mut layout.split);
                        }
                    });
                }
            });
        });
}

enum Pick {
    /// Person and ID.
    Reply(Id, Id),
    Edit(Id, String),
    Delete(Id),
}

fn composer_id(channel: Id) -> UiId {
    UiId::unique(("composer", channel))
}

/// The latest problem, in red, with a close button.
pub fn toast_line(ui: &mut Ui, toast: &mut Option<(String, f64)>) {
    if let Some((text, _)) = toast.as_ref() {
        let mut close = false;
        ui.horizontal(|ui| {
            ui.colored_label(ui::colors(ui).danger_text, text.as_str());
            close = ui::small_close(ui, t!("common.dismiss")).clicked();
        });
        if close {
            *toast = None;
        }
    }
}

/// Returns a message to send, and whether a file was chosen to attach: stored (`false`) or live.
fn composer(
    ui: &mut Ui,
    chat: &mut Chat,
    dir: &Directory,
    online: bool,
    channel: Id,
    hint: &str,
    emoji_picker: &mut Picker,
) -> (Option<Command>, Option<bool>) {
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
    let ready =
        !chat.draft.trim().is_empty() || (chat.editing.is_none() && !chat.staged.is_empty());
    let mut attach = None;
    let mut send = false;
    let mut tabs = Vec::new();
    let button = Vec2::splat(COMPOSER_BUTTON);
    let bar = Frame::new()
        .fill(ui::colors(ui).composer)
        .corner_radius(ui::RADIUS_WINDOW)
        .inner_margin(Margin::same(ui::SPACE_XS as i8 + 2))
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
                    let upload =
                        Item::new("📎", t!("home.upload_file")).hint(t!("home.upload_file_help"));
                    if m.add(upload).clicked() {
                        attach = Some(false);
                    }
                    let live =
                        Item::new("📡", t!("home.send_live")).hint(t!("home.send_live_help"));
                    if m.add(live).on_hover_text(t!("call.direct_chat")).clicked() {
                        attach = Some(true);
                    }
                });
                // Room for the buttons right of the box: emoji, sticker and send.
                let right = 3.0 * (COMPOSER_BUTTON + 2.0);
                let size = Vec2::new(ui.available_width() - right, 0.0);
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
    let (was_open, picked) = emoji_picker.show(ui, bar.response.rect, dir);
    for (response, tab) in &tabs {
        emoji_picker.button(response, *tab, editor, was_open);
    }
    let mut sticker = None;
    match picked {
        Some(Picked::Text(sequence)) => emoji_picker.insert(&field, &mut chat.draft, &sequence),
        Some(Picked::Sticker(id, name)) => sticker = Some((id, name)),
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
    if let Some((id, name)) = sticker {
        let Ok(message) = rusttalk_core::keys::random_key() else {
            tracing::warn!("no randomness for a message ID");
            return (None, attach);
        };
        let command = Command::Send(Draft {
            id: *message,
            channel,
            peer: None,
            text: name,
            reply_to: chat.reply_to.take(),
            edit_of: None,
            revision: 0,
            files: Vec::new(),
            sticker: Some(id),
            friend: None,
            poke: false,
        });
        field.request_focus();
        return (Some(command), attach);
    }
    let parser_input = chat.mentions.parser_input(&chat.draft);
    let converted = markdown::cached(ui, &parser_input).map_source(&chat.draft, |s| {
        emoji::replace(&chat.mentions.encode(s), &dir.emoji).into_owned()
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
        id: *id,
        channel,
        peer: None,
        text: text.to_owned(),
        reply_to: chat.reply_to.take(),
        edit_of,
        revision: edit_of.map_or(0, |original| chat.next_revision(dir.me, original, now_ms)),
        files,
        sticker: None,
        friend: None,
        poke: false,
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
                    .frame(Frame::NONE.inner_margin(Margin::symmetric(6, 9)))
                    .hint_text(if online {
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

fn message(
    ui: &mut Ui,
    chat: &Chat,
    msg: &Msg,
    start: bool,
    dir: &Directory,
    channel: Id,
    attachments: impl FnOnce(&mut Ui),
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
        let name = dir
            .members
            .iter()
            .find(|member| member.person == msg.person)
            .and_then(|member| member.name.as_deref())
            .unwrap_or(name);
        ui.add_space(4.0);
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
        ui.add_space(4.0);
        return None;
    }
    if let Body::Friend(signal, _) = msg.body {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.weak("•");
            let line = friends::line(signal, &names(&msg.person));
            ui.label(emoji::text(line).weak().italics());
            ui.weak(time(msg.at));
        });
        ui.add_space(4.0);
        return None;
    }
    let mention = mentions::Message::cached(ui, msg, dir, channel);
    let my_roles = dir
        .members
        .iter()
        .find(|member| member.person == me)
        .map_or(&[][..], |member| member.roles.as_slice());
    let pinged = mentions::mentions_me(&mention, &me, my_roles);
    let background = ui.painter().add(Shape::Noop);
    let name = names(&msg.person);
    if start {
        ui.add_space(10.0);
    }
    // The row senses clicks behind its contents, for the right-click menu. Not a Tab
    // stop: the hover toolbar has the same actions as buttons.
    let row = ui.scope_builder(UiBuilder::new().sense(Sense::CLICK), |ui| {
        ui.horizontal_top(|ui| {
            if start {
                face(ui, dir, &msg.person, &name, MESSAGE_AVATAR, false);
            } else {
                ui.add_space(MESSAGE_AVATAR + ui.spacing().item_spacing.x);
            }
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                if let Some(reply) = msg.reply_to {
                    let quoted = chat.quoted(&reply);
                    let line = match quoted {
                        Some(quoted) if quoted.sticker.is_some() => {
                            format!(
                                "↪ {}",
                                t!("home.quote_sticker", name = names(&quoted.person))
                            )
                        }
                        Some(quoted) if matches!(quoted.body, Body::Text(_)) => {
                            let text =
                                mentions::Message::cached(ui, quoted, dir, channel).plain(dir);
                            let person = quoted.person;
                            let snippet: String = text.chars().take(80).collect();
                            let snippet = if snippet.is_empty() {
                                t!("home.a_file").into()
                            } else {
                                snippet
                            };
                            format!("↪ {}: {}", names(&person), snippet.replace('\n', " "))
                        }
                        _ => format!("↪ {}", t!("home.earlier_message")),
                    };
                    ui.label(emoji::text(line).size(ui::TEXT_CAPTION).weak());
                }
                if start {
                    ui.horizontal(|ui| {
                        ui.label(ui::bold(&name));
                        ui.label(ui::caption(time(msg.at)));
                    });
                }
                match &msg.body {
                    Body::Text(name) if msg.sticker.is_some() => {
                        sticker(ui, dir, msg.sticker.as_ref(), name);
                    }
                    Body::Text(text) => {
                        if !text.is_empty() {
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
                            });
                        }
                        attachments(ui);
                    }
                    _ => {
                        ui.label(
                            RichText::new(t!("home.message_unreadable"))
                                .italics()
                                .weak(),
                        );
                    }
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
    let can_reply = text.is_some();
    let can_edit = mine && text.is_some() && msg.sticker.is_none();
    // A DM copy that didn't open proves nothing about who wrote it, and deleting a
    // DM message reaches every server, so only a readable copy of mine can be deleted.
    let dm_copy = !msg.replicas.is_empty();
    let authored = mine && !(dm_copy && msg.body == Body::Unreadable);
    let can_delete =
        authored || (dir.perms.delete_messages && !dir.dms.iter().any(|dm| dm.channel == channel));
    let mut pick = None;
    ui::context_menu(&row.response, |m| {
        if can_reply && m.item("↩", t!("common.reply")).clicked() {
            pick = Some(Pick::Reply(msg.person, msg.id));
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
        if can_delete {
            m.separator();
            if m.danger("🗑", t!("common.delete")).clicked() {
                pick = Some(Pick::Delete(msg.id));
            }
        }
    });
    let hovered = ui.rect_contains_pointer(rect);
    if pinged {
        ui::mention_highlight(ui, background, rect.expand(2.0), hovered);
    } else if hovered {
        ui.painter().set(
            background,
            Shape::rect_filled(rect.expand(2.0), ui::RADIUS_S, ui::colors(ui).hover_soft),
        );
    }
    if !hovered {
        return pick;
    }
    // A small floating toolbar on the top right of the row.
    let buttons = [can_reply, can_edit, can_delete]
        .into_iter()
        .filter(|on| *on)
        .count() as f32;
    if buttons == 0.0 {
        return pick;
    }
    let size = Vec2::new(
        buttons * TOOLBAR_BUTTON.x + (buttons - 1.0) * 2.0 + 8.0,
        TOOLBAR_BUTTON.y + 8.0,
    );
    let bar = Rect::from_min_size(rect.right_top() + Vec2::new(-size.x - 8.0, -14.0), size);
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
    // A child that takes no space, so the toolbar floats over the row.
    let mut ui = ui.new_child(
        UiBuilder::new()
            .max_rect(bar.shrink(4.0))
            .layout(Layout::left_to_right(Align::Center)),
    );
    ui.spacing_mut().item_spacing.x = 2.0;
    if can_reply
        && ui::icon_button(
            &mut ui,
            "↩",
            t!("common.reply"),
            Look::Ghost,
            TOOLBAR_BUTTON,
        )
        .clicked()
    {
        pick = Some(Pick::Reply(msg.person, msg.id));
    }
    if can_edit
        && let Some(text) = &text
        && ui::icon_button(&mut ui, "✏", t!("common.edit"), Look::Ghost, TOOLBAR_BUTTON).clicked()
    {
        pick = Some(Pick::Edit(msg.id, edit_text(text)));
    }
    if can_delete
        && ui::icon_button(
            &mut ui,
            "🗑",
            t!("common.delete"),
            Look::Ghost,
            TOOLBAR_BUTTON,
        )
        .clicked()
    {
        pick = Some(Pick::Delete(msg.id));
    }
    pick
}

/// A sticker message: the picture, large, or its name when not available.
fn sticker(ui: &mut Ui, dir: &Directory, id: Option<&Id>, name: &str) {
    let code = format!(":{name}:");
    match id
        .and_then(|id| dir.emoji_by_id(id))
        .and_then(|e| e.picture.as_ref())
    {
        Some(picture) => {
            ui.add(
                egui::Image::new(&frame(ui, picture))
                    .fit_to_exact_size(Vec2::splat(STICKER_SIZE))
                    .alt_text(t!("home.sticker", code = code)),
            )
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
        Dialog::RenameChannel { .. } => t!("home.edit_channel").into(),
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
        Dialog::Leave(index, _) => t!("home.leave_title", server = home.servers[*index].name()),
        Dialog::Poke(person, _) => t!("pokes.poke_title", name = home.peer_name(person)),
    }
}

fn dialogs(ctx: &egui::Context, home: &mut Home) {
    let Some(dialog) = &home.dialog else {
        return;
    };
    let managed = match dialog {
        Dialog::RenameChannel { server, .. }
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
    let mut poke = None;
    // Message (`None`) or a friend action, from a person's details.
    let mut person_action = None;
    let ((), dismissed) = ui::dialog(ctx, "dialog", &title, |ui| match dialog {
        Dialog::ServerName(index, name) => {
            ui.label(t!("home.server_name_help"));
            if text_dialog(ui, name, t!("common.save"), &mut close) {
                command = Some((*index, Command::SetServerName(name.trim().to_owned())));
            }
        }
        Dialog::MyName(index, name) => {
            ui.label(t!("home.my_name_help"));
            if text_dialog(ui, name, t!("common.save"), &mut close) {
                command = Some((*index, Command::SetDisplayName(name.trim().to_owned())));
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
            voice,
            category,
            new_category,
        } => {
            let was_voice = *voice;
            ui.horizontal(|ui| {
                ui.selectable_value(voice, false, format!("# {}", t!("home.kind_text")));
                ui.selectable_value(voice, true, format!("🔊 {}", t!("home.kind_voice")));
            });
            let data = &home.servers[*server];
            // Switching kind moves a default pick along, but keeps one the person made.
            if *voice != was_voice && *category == default_category(data, was_voice) {
                *category = default_category(data, *voice);
                if *new_category == category_name(was_voice) {
                    *new_category = category_name(*voice).into();
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
                        voice: *voice,
                        name: name.trim().to_owned(),
                    },
                ));
            }
        }
        Dialog::RenameChannel {
            server,
            id,
            name,
            topic,
            topic_changed,
            audio,
        } => {
            let label = ui.label(t!("home.channel_name")).id;
            let name_field = name_field(ui, name).labelled_by(label);
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
            }
            let submitted = submitted(ui, &name_field) || submitted(ui, &field);
            if text_footer(ui, t!("common.save"), submitted, &mut close) && !name.trim().is_empty()
            {
                command = Some((
                    *server,
                    Command::RenameChannel {
                        id: *id,
                        name: name.trim().to_owned(),
                        topic: topic_changed.then(|| topic.trim().to_owned()),
                        audio_kbps: audio.and_then(|(kbps, changed)| changed.then_some(kbps)),
                    },
                ));
            }
        }
        Dialog::RenameCategory(index, id, name) => {
            let label = ui.label(t!("home.category_name")).id;
            let field = name_field(ui, name).labelled_by(label);
            let submitted = submitted(ui, &field);
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
                    let msg = home.servers[*index]
                        .chats
                        .get(channel)
                        .and_then(|chat| chat.messages.iter().find(|msg| msg.id == *id));
                    let files = msg
                        .into_iter()
                        .flat_map(|msg| &msg.files)
                        .flat_map(|file| std::iter::once(&file.file).chain(&file.thumbnail))
                        .map(|blob| blob.id)
                        .collect();
                    let sender = msg.map_or([0; 32], |msg| msg.person);
                    command = Some((*index, Command::Delete(*channel, *id, sender, files)));
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
}

/// A name field with a confirm button; returns true when confirmed with a non-empty name.
fn text_dialog(ui: &mut Ui, text: &mut String, confirm: &str, close: &mut bool) -> bool {
    let field = name_field(ui, text);
    let submitted = submitted(ui, &field);
    text_footer(ui, confirm, submitted, close) && !text.trim().is_empty()
}

fn name_field(ui: &mut Ui, text: &mut String) -> Response {
    let field = ui.add(ui::field(text).char_limit(100).desired_width(f32::INFINITY));
    if ui.memory(|memory| memory.focused().is_none()) {
        field.request_focus();
    }
    field
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
}
