//! The signed-in window: server rail, channels, chat and members.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use chrono::{DateTime, Local};
use eframe::egui::{
    self, Align, Align2, CentralPanel, CollapsingHeader, Color32, FontId, Frame, Id as UiId, Key,
    KeyboardShortcut, Label, Layout, Margin, Modifiers, Panel, Rect, Response, RichText, Role,
    ScrollArea, Sense, Shape, TextEdit, TextureHandle, TextureOptions, Ui, UiBuilder, Vec2,
    WidgetInfo, ecolor::Hsva,
};
use rusttalk_core::{
    auth::{Fingerprint, InviteLink, decode},
    client::{MAX_TEXT_CHARS, voice::LiveFileEvent},
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
    files::{self, Ask, Files},
    net::{Admin, AdminUpdate, Admission, Command, Draft, Listed, Net, PAGE_SIZE, Preview, Update},
    official, reorder,
    screen::Video,
    server::{Body, Chat, Directory, Id, Member, Msg, Picture, Server, Status, member_name},
    ui::{self, Item, Look, Palette, error_text},
};

/// The name shown until a server has one, or until its key arrives.
/// Text fields in settings rows.
const FIELD_WIDTH: f32 = 280.0;
const NEW_SERVER: &str = "New server";
/// Show the character count once a message gets this long.
const COUNT_FROM: usize = 1_800;
const DAY: u32 = 24 * 60 * 60;
/// Seconds without any input before "Online" turns into "Away" by itself.
const AWAY_AFTER: f64 = 10.0 * 60.0;
const RAIL_WIDTH: f32 = 72.0;
const RAIL_BUTTON: f32 = 48.0;
const SERVER_HEADER: f32 = 44.0;
const MESSAGE_AVATAR: f32 = 38.0;
const TOOLBAR_BUTTON: Vec2 = Vec2::new(30.0, 26.0);

pub mod layout;
mod user_bar;

pub struct Home {
    pub identity: Arc<DeviceIdentity>,
    net: Net,
    pub servers: Vec<Server>,
    view: View,
    invite: String,
    join: Join,
    find: Find,
    dialog: Option<Dialog>,
    pub toast: Option<(String, f64)>,
    pub settings: Option<Tab>,
    pub voice: Voice,
    pub files: Files,
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
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Join,
    /// The public server directory.
    Find,
    /// A DM: server index and channel.
    Direct(Option<(usize, Id)>),
    Server(usize),
}

/// The "Find servers" page.
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
    Checking,
    Preview { preview: Box<Preview>, name: String },
    Joining(url::Url),
}

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
    },
    RenameCategory(usize, Id, String),
    DeleteItem(usize, reorder::Item, String),
    Invite(Invite),
    Member(usize, Id),
    Delete(usize, Id, Id),
    /// Leave a server, with the reason it didn't work.
    Leave(usize, Option<String>),
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
                    NEW_SERVER,
                    net.start(known, None, None),
                )
            })
            .collect();
        let runtime = net.handle();
        let synced = device_sync::synced(&audio.settings());
        let layout = layout::Layout::new(&audio.settings());
        // Without servers yet, start by browsing the directory.
        let browse = servers.is_empty();
        Ok(Self {
            identity,
            net,
            view: if browse { View::Find } else { View::Server(0) },
            servers,
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
        self.view = match self.view {
            View::Server(i) | View::Direct(Some((i, _))) if i == index => {
                if self.servers.is_empty() {
                    self.find.open();
                    View::Find
                } else {
                    View::Server(0)
                }
            }
            View::Server(i) if i > index => View::Server(i - 1),
            View::Direct(Some((i, channel))) if i > index => View::Direct(Some((i - 1, channel))),
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

    /// Publish the synced settings that changed since the last sync.
    pub fn publish_settings(&mut self) {
        let current = device_sync::synced(&self.voice.audio.settings());
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
                        self.join = Join::Idle(Some(error.clone()));
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
                    // Opens that failed while offline.
                    for (channel, chat) in &server.chats {
                        if chat.loading && chat.messages.is_empty() {
                            let _ = server.commands.send(Command::Open(*channel));
                        }
                    }
                }
                server.status = status;
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
                    server.dir = *dir;
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
            Update::Deleted(address, channel, id) => {
                let Some(index) = self.index(&address) else {
                    return;
                };
                if let Some(chat) = self.servers[index].chats.get_mut(&channel) {
                    chat.remove(&id);
                    if chat.reply_to == Some(id) {
                        chat.reply_to = None;
                    }
                    if chat.editing == Some(id) {
                        chat.editing = None;
                        chat.draft.clear();
                    }
                }
            }
            Update::Notice(address, person, msg) => {
                let Some(index) = self.index(&address) else {
                    return;
                };
                let server = &mut self.servers[index];
                for (channel, chat) in &mut server.chats {
                    let dm = server.dir.dms.iter().find(|dm| dm.channel == *channel);
                    if dm.is_none_or(|dm| dm.peer == person) {
                        chat.merge(vec![msg.clone()], false);
                    }
                }
            }
            Update::DmReady(address, channel) => {
                if let Some(index) = self.index(&address) {
                    self.view = View::Direct(Some((index, channel)));
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
                    Some(state) => call.state = state,
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
            Update::Room(address, channel, peers) => {
                if let Some(index) = self.index(&address) {
                    let rooms = &mut self.servers[index].voice;
                    if peers.is_empty() {
                        rooms.remove(&channel);
                    } else {
                        rooms.insert(channel, peers);
                    }
                }
            }
            Update::AppStopped(id) => self.voice.shares.retain(|app| app.id != id),
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
                    self.servers[index]
                        .chats
                        .entry(channel)
                        .or_default()
                        .uploading = what;
                }
            }
            Update::Image(id, image) => self.files.image(ctx, id, image),
            Update::Saved(id, saved) => self.files.saved(id, saved),
            Update::Synced(synced) => {
                for known in &synced.servers {
                    if self.index(&known.address).is_none() && !self.left.contains(&known.address) {
                        let commands = self.net.start(known.clone(), None, None);
                        self.servers
                            .push(new_server(known.address.clone(), NEW_SERVER, commands));
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
                let audio = &self.voice.audio;
                let adopted = device_sync::adopt(&audio.settings(), &synced.settings);
                self.synced = device_sync::synced(&adopted);
                audio.set_settings(adopted);
            }
            Update::Unsent(address, draft) => {
                let Some(index) = self.index(&address) else {
                    return;
                };
                let chat = self.servers[index].chats.entry(draft.channel).or_default();
                // Only when nothing new was typed or picked meanwhile.
                if chat.draft.is_empty() && chat.staged.is_empty() {
                    chat.draft = draft.text;
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
    while let Ok(update) = home.net.updates.try_recv() {
        home.apply(ui.ctx(), update, now);
    }
    home.track_activity(ui);
    ui::apply(ui.ctx(), home.voice.audio.settings().appearance());
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
        let (Ask::Attach(address, _)
        | Ask::Save(address, _)
        | Ask::Live(address, _)
        | Ask::Icon(address)
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
                    home.toast = Some(("Live files can be up to 2 GB.".into(), now + 6.0));
                } else {
                    server.send(Command::SendLive(channel, path));
                }
            }
            Ask::Picture => {}
            Ask::Accept(_, id, _) => {
                let Some(path) = paths.into_iter().next() else {
                    continue;
                };
                // Receiving never replaces a file, even one the dialog offered to overwrite.
                if path.exists() {
                    home.toast = Some((
                        "That file already exists. Pick a new name.".into(),
                        now + 6.0,
                    ));
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
    if home.voice.watching.as_ref().is_some_and(|w| w.full) {
        CentralPanel::default()
            .frame(panel_frame(colors.main))
            .show(ui, |ui| call::screen_view(ui, home));
        call::frame(ui.ctx(), home);
        return None;
    }
    Panel::left("servers")
        .resizable(false)
        .exact_size(RAIL_WIDTH)
        .frame(panel_frame(colors.rail))
        .show(ui, |ui| server_rail(ui, home));
    home.layout.set_window_width(ui.available_width());
    let channels = Panel::left("channels")
        .resizable(false)
        .exact_size(home.layout.channels)
        .frame(panel_frame(colors.side))
        .show(ui, |ui| sidebar(ui, home, updates.available()))
        .response
        .rect;
    let mut members = None;
    if let View::Server(index) = home.view
        && home.servers[index].status != Status::Connecting
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
            View::Direct(None) => empty(
                ui,
                "Direct messages",
                "Pick a person in a server's member list to start a chat.",
            ),
            View::Direct(Some((index, channel))) => chat_pane(ui, home, index, channel),
            View::Server(index) => match home.servers[index].open {
                Some(channel) if home.servers[index].is_voice(&channel) => {
                    call::room(ui, home, index, channel)
                }
                Some(channel) => chat_pane(ui, home, index, channel),
                None => {
                    let server = &home.servers[index];
                    let hint = match &server.status {
                        Status::Stopped(reason) => reason.clone(),
                        Status::Connecting => "Connecting…".into(),
                        _ if server.dir.categories.is_empty()
                            && server.dir.perms.manage_channels =>
                        {
                            "No channels yet. Create one from the server menu.".into()
                        }
                        _ => "Pick a channel on the left.".into(),
                    };
                    empty(ui, server.name(), &hint);
                }
            },
        });
    // Resize handles go last so they sit above the panels on both sides of each edge.
    let mut dragging = layout::resize_edge(
        ui,
        "Resize channels",
        channels,
        true,
        &mut home.layout.channels,
        layout::CHANNELS_WIDTH,
        layout::CHANNELS_RANGE,
    );
    if let Some(members) = members {
        dragging |= layout::resize_edge(
            ui,
            "Resize members",
            members,
            false,
            &mut home.layout.members,
            layout::MEMBERS_WIDTH,
            layout::MEMBERS_RANGE,
        );
    }
    if !dragging {
        home.layout.save(&home.voice.audio);
    }
    dialogs(ui.ctx(), home);
    admin::window(ui.ctx(), home);
    files::viewer(ui.ctx(), &mut home.files);
    call::screen_dialog(ui.ctx(), home);
    call::frame(ui.ctx(), home);
    let action = crate::settings::show(ui.ctx(), home, updates);
    crop_dialog(ui.ctx(), home);
    action
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
        if rail_button(
            ui,
            "💬",
            "Direct messages",
            matches!(home.view, View::Direct(_)),
        )
        .clicked()
        {
            home.view = View::Direct(None);
        }
        ui.separator();
        for index in 0..home.servers.len() {
            let server = &home.servers[index];
            let selected = home.view == View::Server(index);
            let button = match &server.dir.icon {
                Some(icon) => {
                    let image = egui::Image::new(&frame(ui, icon))
                        .fit_to_exact_size(Vec2::splat(RAIL_BUTTON))
                        .corner_radius(if selected { 16 } else { 24 })
                        .sense(Sense::click());
                    let button = ui.add(image).on_hover_text(server.name());
                    let name = server.name().to_owned();
                    button.widget_info(|| WidgetInfo::labeled(Role::Button, true, &name));
                    button
                }
                None => rail_button(ui, &initials(server.name()), server.name(), selected),
            };
            if button.clicked() {
                home.view = View::Server(index);
            }
        }
        let adding = matches!(home.view, View::Join | View::Find);
        let plus = rail_button(ui, "+", "Add a server", adding);
        ui::menu(&plus, |m| {
            if m.item("🔗", "Join with an invite").clicked() {
                home.view = View::Join;
            }
            if m.item("🔍", "Find servers").clicked() {
                home.view = View::Find;
                home.find.open();
            }
        });
    });
}

fn sidebar(ui: &mut Ui, home: &mut Home, update_available: bool) {
    Panel::bottom("you")
        .frame(Frame::new().inner_margin(Margin::symmetric(0, 6)))
        .show(ui, |ui| {
            call::call_bar(ui, home);
            user_bar::show(ui, home, update_available);
        });
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
    }
}

/// The texture for a picture's current frame, so animated GIFs play.
// shortcut: textures stay in egui's memory until restart; evict them if people change pictures often.
pub fn frame(ui: &Ui, picture: &Picture) -> TextureHandle {
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
    let total: u64 = picture.frames.iter().map(|(_, ms)| u64::from(*ms)).sum();
    if textures.len() > 1 && total > 0 {
        let mut at = (ui.input(|input| input.time) * 1000.0) as u64 % total;
        for (texture, (_, ms)) in textures.iter().zip(picture.frames.iter()) {
            let ms = u64::from(*ms);
            if at < ms {
                ui.ctx()
                    .request_repaint_after(Duration::from_millis(ms - at));
                return texture.clone();
            }
            at -= ms;
        }
    }
    textures[0].clone()
}

fn dm_list(ui: &mut Ui, home: &mut Home) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(ui::SPACE_S);
        ui.label(ui::title("Direct messages"));
    });
    ui.separator();
    let mut any = false;
    ScrollArea::vertical().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for (index, server) in home.servers.iter().enumerate() {
            for dm in &server.dir.dms {
                any = true;
                let selected = home.view == View::Direct(Some((index, dm.channel)));
                let name = server.member_name(&dm.peer);
                if selected {
                    let rect =
                        Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 44.0));
                    ui.painter()
                        .rect_filled(rect, ui::RADIUS_ROW, ui::colors(ui).selected);
                }
                let row = ui::row_button(ui, 44.0, &name, |ui| {
                    face(ui, &server.dir, &dm.peer, &name, 30.0, true);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.add(Label::new(ui::bold(&name)).truncate());
                        ui.add(Label::new(ui::caption(server.name())).truncate());
                    });
                });
                if row.clicked() {
                    home.view = View::Direct(Some((index, dm.channel)));
                }
            }
        }
    });
    if !any {
        ui.weak("No chats yet.");
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
        FontId::new(ui::TEXT_TITLE - 1.0, ui::bold_family()),
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
    let label = format!("{name}, server menu");
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
                ui.weak("Connecting…");
            });
        }
        Status::Offline => {
            ui.weak("Offline. Trying again…");
        }
        Status::Stopped(reason) => {
            error_text(ui, Some(reason));
            if ui::secondary(ui, "Remove server").clicked() {
                home.leave_server(index);
                return;
            }
        }
        Status::Online => {}
    }
    if online && perms.owner && server.dir.name.is_none() {
        ui.add_space(6.0);
        if ui::primary(ui, "Name your server").clicked() {
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
    let categories = &server.dir.categories;
    let can_manage = perms.manage_channels && online;
    let can_move = can_manage && reorder::order(categories).is_some();
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
                                    &format!("Create channel in {}", category.name),
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
                        if m.item("✏", "Rename category").clicked() {
                            category_dialog =
                                Some(Dialog::RenameCategory(index, id, category.name.clone()));
                        }
                        if can_move {
                            moved = moved.or(reorder::menu(m, categories, item));
                        }
                        m.separator();
                        if m.danger("🗑", "Delete category").clicked() {
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
                if perms.manage_channels {
                    ui::context_menu(&button, |m| {
                        if m.item("⚙", "Channel settings").clicked() {
                            configure = Some(channel.id);
                        }
                        if can_manage {
                            if m.item("✏", "Rename channel").clicked() {
                                channel_edit = Some((channel.id, ChannelEdit::Rename));
                            }
                            if can_move {
                                moved = moved.or(reorder::menu(m, categories, item));
                            }
                            m.separator();
                            if m.danger("🗑", "Delete channel").clicked() {
                                channel_edit = Some((channel.id, ChannelEdit::Delete));
                            }
                        }
                    });
                }
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
    let mut grouped = false;
    if perms.invite && m.item("👤", "Invite people").clicked() {
        home.dialog = Some(Dialog::Invite(Invite {
            server: index,
            uses: 10,
            seconds: DAY * 7,
            making: false,
            link: None,
        }));
    }
    if perms.settings() && m.item("⚙", "Server settings").clicked() {
        admin::open(home, index, admin::Tab::Members);
    }
    grouped |= perms.invite || perms.settings();
    if perms.manage_channels || perms.manage_server {
        if grouped {
            m.separator();
        }
        if perms.manage_channels {
            if m.item("#", "Create channel").clicked() {
                home.dialog = Some(new_channel(&home.servers[index], index, None));
            }
            if m.item("🗀", "Create category").clicked() {
                home.dialog = Some(Dialog::Category(index, String::new()));
            }
        }
        if perms.manage_server && m.item("✏", "Rename server").clicked() {
            let name = home.servers[index].dir.name.clone().unwrap_or_default();
            home.dialog = Some(Dialog::ServerName(index, name));
        }
        grouped = true;
    }
    if grouped {
        m.separator();
    }
    if m.item("🏷", "Change my name").clicked() {
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
    if m.danger("🚪", "Leave server").clicked() {
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
        "Voice channels"
    } else {
        "Text channels"
    }
}

pub fn avatar(ui: &mut Ui, person: &Id, name: &str, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let color: Color32 = Hsva::new(f32::from(person[0]) / 255.0, 0.45, 0.6, 1.0).into();
    ui.painter().circle_filled(rect.center(), size / 2.0, color);
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        initials(name),
        FontId::new(size * 0.38, ui::bold_family()),
        ui::ON_FILL,
    );
    response
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
        Some(picture) => ui.add(
            egui::Image::new(&frame(ui, picture))
                .fit_to_exact_size(Vec2::splat(size))
                .corner_radius(size / 2.0)
                .sense(Sense::click()),
        ),
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
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(ui::SPACE_XS);
        ui::caption_label(ui, &format!("Members — {}", server.dir.members.len()));
    });
    let perms = server.dir.perms;
    let mut action = None;
    let (mut manage, mut moderate) = (None, None);
    ScrollArea::vertical().show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for member in &server.dir.members {
            let name = member.display_name();
            let me = member.person == server.dir.me;
            let label = if me {
                format!("{name} (you)")
            } else {
                name.clone()
            };
            let row = ui::row_button(ui, 40.0, &label, |ui| {
                member_face(ui, Some(member), &member.person, &name, 30.0, true);
                let text = if member.presence == Presence::Offline {
                    RichText::new(&label).color(ui::colors(ui).muted)
                } else {
                    RichText::new(&label)
                };
                ui.add(Label::new(text).truncate());
            });
            ui::menu(&row, |m| {
                if !me && m.item("💬", "Message").clicked() {
                    action = Some(Command::OpenDm(member.person));
                }
                if m.item("ℹ", "Details").clicked() {
                    home.dialog = Some(Dialog::Member(index, member.person));
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
    });
    if let Some(command) = action {
        home.servers[index].send(command);
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
                ui.label(ui::heading(if home.servers.is_empty() {
                    "No servers yet"
                } else {
                    "Join a server"
                }));
                ui.label("Paste an invite link from a friend.");
                ui.add_space(12.0);
                let label = ui.label("Invite link");
                let field = ui
                    .add(
                        ui::field(&mut home.invite)
                            .hint_text("rusttalk://…")
                            .desired_width(400.0),
                    )
                    .labelled_by(label.id);
                error_text(ui, error.as_deref());
                if ui::primary_large(ui, "Join a server").clicked() || submitted(ui, &field) {
                    match InviteLink::parse(home.invite.trim()) {
                        Ok(link) => {
                            home.net.preview(link);
                            home.join = Join::Checking;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "invite link rejected");
                            *error_slot(&mut home.join) = Some(
                                "That isn't an invite link. Ask your friend to send it again."
                                    .into(),
                            );
                        }
                    }
                }
                official_servers(ui, home);
            }
            Join::Checking => {
                ui.spinner();
                ui.label("Checking the server…");
            }
            Join::Joining(_) => {
                ui.spinner();
                ui.label("Joining…");
            }
            Join::Preview { preview, name } => {
                let address = preview.server.address.clone();
                if let Some(index) = home.servers.iter().position(|s| s.address == address) {
                    ui.label(ui::heading("You're already in this server"));
                    if ui::primary_large(ui, "Open it").clicked() {
                        home.view = View::Server(index);
                        home.join = Join::Idle(None);
                    }
                    return;
                }
                match &preview.name {
                    Some(server) => ui.label(ui::heading(format!("Join {server}?"))),
                    None => ui.label(ui::heading("Join this server?")),
                };
                ui.weak(host(&address));
                ui.add_space(12.0);
                let label = ui.label("Your name on this server");
                let field = ui
                    .add(
                        ui::field(name)
                            .hint_text("What friends call you")
                            .desired_width(300.0),
                    )
                    .labelled_by(label.id);
                ui.add_space(8.0);
                if preview.invite.is_none() && !preview.open {
                    ui.label("This server needs an invite. Ask someone there for a link.");
                    if ui::secondary_large(ui, "Back").clicked() {
                        home.join = Join::Idle(None);
                    }
                    return;
                }
                if preview.invite.is_none() {
                    ui.label("Anyone can join this server, no invite needed.");
                } else if !preview.known {
                    ui.label("First time here. Your invite confirms this is the right server.");
                }
                CollapsingHeader::new("Details").show(ui, |ui| {
                    ui.label(format!("Address  {address}"));
                    ui.label(format!(
                        "Server ID  {}",
                        fingerprint(&preview.server.server_id)
                    ));
                    if let Some(tls) = &preview.server.tls_fingerprint {
                        ui.label(format!("Certificate  {}", fingerprint(tls)));
                    }
                    ui.weak("RustTalk remembers these and stops if they ever change.");
                });
                ui.add_space(8.0);
                if ui::primary_large(ui, "Join").clicked() || submitted(ui, &field) {
                    let name = name.trim();
                    let admission = match preview.invite {
                        Some(code) => Admission::Invite(code),
                        None => Admission::Open,
                    };
                    let commands = home.net.start(
                        preview.server.clone(),
                        Some(admission),
                        (!name.is_empty()).then(|| name.to_owned()),
                    );
                    let label = preview.name.as_deref().unwrap_or(NEW_SERVER);
                    home.servers
                        .push(new_server(address.clone(), label, commands));
                    home.left.remove(&address);
                    home.join = Join::Joining(address);
                } else if ui::secondary_large(ui, "Cancel").clicked() {
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
        ui.label(ui::title("Official servers"));
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
                    let join = ui::primary(ui, "Join");
                    join.widget_info(|| {
                        WidgetInfo::labeled(Role::Button, true, format!("Join {}", listed.name))
                    });
                    let dismiss = ui::secondary(ui, "Dismiss");
                    dismiss.widget_info(|| {
                        WidgetInfo::labeled(Role::Button, true, format!("Dismiss {}", listed.name))
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
    if !settings.dismissed_official.is_empty() && ui.link("Show official servers again").clicked() {
        settings.dismissed_official.clear();
        changed = true;
    }
    if changed {
        audio.set_settings(settings);
        home.publish_settings();
    }
    if let Some(listed) = pick {
        home.join = Join::Checking;
        home.net.check_listed(listed);
    }
}

pub fn privacy_tab(ui: &mut Ui, home: &mut Home) {
    let audio = home.voice.audio.clone();
    let mut settings = audio.settings();
    ui::page_section(ui, "Server directory");
    ui::banner(
        ui,
        "privacy-directory",
        "A directory lists public servers you can find and join. RustTalk only contacts it \
         while you have Find servers open.",
    );
    ui::setting_row(
        ui,
        "Directory address",
        "Leave it empty to use the official directory.",
        |ui| {
            ui.add(
                ui::field(&mut settings.index_url)
                    .hint_text(official::DIRECTORY)
                    .desired_width(FIELD_WIDTH),
            )
            .widget_info(|| WidgetInfo::labeled(Role::TextInput, true, "Directory address"));
        },
    );
    ui::setting_row(
        ui,
        "Certificate fingerprint",
        "Only for a directory without a domain.",
        |ui| {
            ui.add(ui::field(&mut settings.index_fingerprint).desired_width(FIELD_WIDTH))
                .widget_info(|| {
                    WidgetInfo::labeled(Role::TextInput, true, "Certificate fingerprint")
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
    let url = https_url(url).ok_or("Use an address that starts with https://.")?;
    let pin = settings.index_fingerprint.trim();
    let pin = (!pin.is_empty())
        .then(|| decode(pin).map_err(|_| "That certificate fingerprint isn't right."))
        .transpose()?;
    Ok((url, pin))
}

fn find(ui: &mut Ui, home: &mut Home) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui::SPACE_XL);
        ui.set_max_width(640.0);
        ui.label(ui::heading("Find servers"));
        if ui.link("Join with an invite").clicked() {
            home.view = View::Join;
        }
        let settings = home.voice.audio.settings();
        let find = &mut home.find;
        let mut page = find.auto_page();
        ui.horizontal(|ui| {
            let field = ui.add(
                ui::field(&mut find.query)
                    .hint_text("Search by name or description")
                    .desired_width(ui.available_width() - 90.0),
            );
            field.widget_info(|| WidgetInfo::labeled(Role::TextInput, true, "Search"));
            if ui::primary(ui, "Search").clicked() || submitted(ui, &field) {
                page = Some(1);
            }
        });
        error_text(ui, find.error.as_deref());
        if find.error.is_some() && !find.searching && ui::secondary(ui, "Try again").clicked() {
            page = Some(1);
        }
        if find.searching || find.checking {
            ui.spinner();
        } else if find.results.is_empty() && find.page > 0 {
            ui.weak("No servers found.");
        }
        let mut pick = None;
        ScrollArea::vertical().show(ui, |ui| {
            ui.with_layout(Layout::top_down(Align::Min), |ui| {
                for listed in &find.results {
                    ui::card(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(ui::title(&listed.name));
                            if listed.open {
                                ui::badge(ui, "Open joining", ui::accent(ui).base);
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let join = ui
                                    .add_enabled_ui(!find.checking, |ui| ui::primary(ui, "Join"))
                                    .inner;
                                join.widget_info(|| {
                                    WidgetInfo::labeled(
                                        Role::Button,
                                        true,
                                        format!("Join {}", listed.name),
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
                        let members = match listed.members {
                            1 => "1 member".to_owned(),
                            count => format!("{count} members"),
                        };
                        ui.label(ui::caption(members));
                    });
                }
                if find.more && !find.searching && ui::secondary(ui, "Show more").clicked() {
                    page = Some(find.page + 1);
                }
            });
        });
        if let Some(listed) = pick {
            find.checking = true;
            find.error = None;
            home.net.check_listed(listed);
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
        dir: Directory::default(),
        chats: HashMap::new(),
        open: None,
        voice: HashMap::new(),
        names: HashMap::new(),
        commands,
    }
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

fn time(at: i64) -> String {
    let Some(at) = DateTime::from_timestamp(at, 0) else {
        return String::new();
    };
    let at = at.with_timezone(&Local);
    if at.date_naive() == Local::now().date_naive() {
        at.format("%H:%M").to_string()
    } else {
        at.format("%-d %b %H:%M").to_string()
    }
}

fn chat_pane(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    let Some((title, dm)) = home.servers[index].chat_title(&channel) else {
        home.servers[index].open = None;
        if matches!(home.view, View::Direct(_)) {
            home.view = View::Direct(None);
        }
        return;
    };
    let title = if dm {
        format!("@ {title}")
    } else {
        format!("# {title}")
    };
    let in_server = matches!(home.view, View::Server(_));
    let Home {
        servers,
        toast,
        dialog,
        files,
        voice,
        layout,
        ..
    } = home;
    let server = &mut servers[index];
    let Server {
        address,
        chats,
        dir,
        status,
        commands,
        ..
    } = server;
    let chat = chats.entry(channel).or_insert_with(|| {
        // First look at this channel: load it, then follow live updates.
        let _ = commands.send(Command::Open(channel));
        let mut chat = Chat::default();
        chat.loading = true;
        chat
    });
    pane_title(ui, "chat-title", &title, in_server.then_some(layout));
    let online = *status == Status::Online;
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
        *toast = Some((problem, ui.input(|input| input.time) + 6.0));
    }
    let attach = Panel::bottom("composer")
        .frame(Frame::new().inner_margin(Margin::symmetric(0, 8)))
        .show(ui, |ui| {
            let (command, attach) = composer(ui, chat, dir, online, toast, channel, &title);
            if let Some(command) = command {
                let _ = commands.send(command);
            }
            attach
        })
        .inner;
    match attach {
        Some(false) => files.ask(ui.ctx(), Ask::Attach(address.clone(), channel)),
        Some(true) => files.ask(ui.ctx(), Ask::Live(address.clone(), channel)),
        None => {}
    }
    if online && hovered {
        let rect = ui.max_rect();
        let accent = ui::accent(ui);
        ui.painter()
            .rect_filled(rect, ui::RADIUS, accent.base.gamma_multiply(0.35));
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            format!("Drop to send in {title}"),
            FontId::new(20.0, ui::bold_family()),
            ui::colors(ui).strong,
        );
    }
    let can_moderate = dir.perms.delete_messages && !dm;
    let mut area = ScrollArea::vertical()
        .id_salt(("chat", index, channel))
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
            ui.label(ui::heading(format!("This is the start of {title}")));
            ui.add_space(ui::SPACE_M);
        } else if chat.loading {
            ui.vertical_centered(|ui| ui.spinner());
        }
        let mut last: Option<&Msg> = None;
        for msg in &chat.messages {
            let start = !last.is_some_and(|prev| {
                prev.person == msg.person
                    && !matches!(prev.body, Body::Notice { .. })
                    && msg.at - prev.at < 300
                    && msg.reply_to.is_none()
            });
            let shown = message(ui, chat, msg, start, dir, can_moderate, |ui| {
                files::attachments(ui, files, address, commands, &msg.files);
                // The server isn't borrowed whole here, so look the offer up by its parts.
                if msg.live.is_some() {
                    let action = call::chat_live(ui, &voice.live_files, dir, msg);
                    live = action.or(live.take());
                }
            });
            if let Some(chosen) = shown {
                pick = Some(chosen);
            }
            last = Some(msg);
        }
    });
    let content = output.content_size.y;
    if let Some(before) = chat.keep_scroll
        && content > before
    {
        chat.keep_scroll = None;
        chat.scroll_to = Some(output.state.clamped_offset().y + content - before);
        ui.ctx().request_repaint();
    }
    // Only page once the layout has settled; the first frame always starts at the top.
    let settled = chat.content_height == content;
    chat.content_height = content;
    if settled
        && output.state.clamped_offset().y < 4.0
        && !chat.loading
        && !chat.at_start
        && chat.keep_scroll.is_none()
    {
        let oldest = chat
            .messages
            .iter()
            .find(|msg| !matches!(msg.body, Body::Notice { .. }));
        if let Some(oldest) = oldest {
            chat.loading = true;
            let _ = commands.send(Command::Older(channel, oldest.id));
        }
    }
    match live {
        Some(LiveAction::Save(id, name)) => {
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
            files.ask(ui.ctx(), Ask::Accept(address.clone(), id, name));
        }
        Some(LiveAction::Remove(id, stop)) => {
            if stop {
                let _ = commands.send(Command::CancelFile(id));
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
        Some(Pick::Reply(id)) => {
            chat.reply_to = Some(id);
            if chat.editing.take().is_some() {
                chat.draft.clear();
            }
            ui.memory_mut(|memory| memory.request_focus(composer_id(channel)));
        }
        Some(Pick::Edit(id, text)) => {
            chat.editing = Some(id);
            chat.reply_to = None;
            chat.draft = text;
            ui.memory_mut(|memory| memory.request_focus(composer_id(channel)));
        }
        Some(Pick::Delete(id)) => *dialog = Some(Dialog::Delete(index, channel, id)),
        None => {}
    }
}

/// The title bar of a pane, like "# general", with a line under it.
/// In a server it also has the button that shows or hides the member list.
pub fn pane_title(
    ui: &mut Ui,
    id: &'static str,
    title: &str,
    members: Option<&mut layout::Layout>,
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
                    });
                }
            });
        });
}

enum Pick {
    Reply(Id),
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
            close = ui::small_close(ui, "Dismiss").clicked();
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
    toast: &mut Option<(String, f64)>,
    channel: Id,
    title: &str,
) -> (Option<Command>, Option<bool>) {
    toast_line(ui, toast);
    let mut cancel = false;
    if let Some(reply) = chat.reply_to {
        let author = chat
            .messages
            .iter()
            .find(|msg| msg.id == reply)
            .map_or_else(|| "a message".into(), |msg| member_name(dir, &msg.person));
        ui.horizontal(|ui| {
            ui.weak(format!("Replying to {author}"));
            cancel = ui::small_close(ui, "Cancel reply").clicked();
        });
    }
    if chat.editing.is_some() {
        ui.horizontal(|ui| {
            ui.weak("Editing message");
            cancel = ui::small_close(ui, "Cancel edit").clicked();
        });
    }
    if let Some((what, done)) = &chat.uploading {
        ui.horizontal(|ui| {
            ui.weak(format!("Uploading {what}"));
            ui.add(
                egui::ProgressBar::new(*done)
                    .show_percentage()
                    .desired_width(200.0),
            );
        });
    }
    files::staged(ui, chat);
    let input = ui::colors(ui).field;
    let mut attach = None;
    let field = ui
        .horizontal_top(|ui| {
            let plus = ui
                .add_enabled_ui(online && chat.editing.is_none(), |ui| {
                    ui::icon_button(ui, "➕", "Attach files", Look::Plain, Vec2::splat(36.0))
                })
                .inner;
            ui::menu_wide(&plus, |m| {
                let upload =
                    Item::new("📎", "Upload a file").hint("Stored on the server, up to 32 MB");
                if m.add(upload).clicked() {
                    attach = Some(false);
                }
                let live = Item::new("📡", "Send file live").hint("Directly to people online now");
                if m.add(live).on_hover_text(call::DIRECT_CHAT).clicked() {
                    attach = Some(true);
                }
            });
            ScrollArea::vertical()
                .id_salt("composer-scroll")
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.add_enabled(
                        online,
                        TextEdit::multiline(&mut chat.draft)
                            .background_color(input)
                            .margin(Margin::symmetric(12, 9))
                            .id(composer_id(channel))
                            .hint_text(if online {
                                format!("Message {title}")
                            } else {
                                "Waiting for the server…".into()
                            })
                            .desired_rows(1)
                            .desired_width(f32::INFINITY)
                            .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter)),
                    )
                })
                .inner
        })
        .inner;
    ui::field_focus(ui, &field);
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
            Body::Text(text) if msg.person == dir.me && msg.live.is_none() => {
                Some((msg.id, text.clone()))
            }
            _ => None,
        });
        if let Some((id, text)) = last {
            chat.editing = Some(id);
            chat.reply_to = None;
            let end = text.chars().count();
            chat.draft = text;
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
            chat.draft.clear();
        }
    }
    let count = chat.draft.chars().count();
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
    let text = chat.draft.trim();
    // Edits change only the text; staged files wait for the next message.
    let editing = chat.editing.is_some();
    let nothing = text.is_empty() && (editing || chat.staged.is_empty());
    if !(focused && enter) || nothing || count > MAX_TEXT_CHARS {
        return (None, attach);
    }
    let files = if editing {
        Vec::new()
    } else {
        chat.staged.drain(..).map(|file| file.path).collect()
    };
    let command = Command::Send(Draft {
        channel,
        text: text.to_owned(),
        reply_to: chat.reply_to.take(),
        edit_of: chat.editing.take(),
        files,
    });
    chat.draft.clear();
    field.request_focus();
    (Some(command), attach)
}

fn message(
    ui: &mut Ui,
    chat: &Chat,
    msg: &Msg,
    start: bool,
    dir: &Directory,
    can_moderate: bool,
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
            ui.label(RichText::new(format!("{name} {action}")).weak().italics());
            ui.weak(time(msg.at));
            ui.add(
                Label::new(RichText::new("Details").small().underline().weak())
                    .sense(Sense::hover()),
            )
            .on_hover_text(detail);
        });
        ui.add_space(4.0);
        return None;
    }
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
                ui.add_space(MESSAGE_AVATAR);
            }
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                if let Some(reply) = msg.reply_to {
                    let quoted = chat.messages.iter().find(|m| m.id == reply);
                    let line = match quoted.map(|m| (&m.body, m.person)) {
                        Some((Body::Text(text), person)) => {
                            let snippet: String = text.chars().take(80).collect();
                            let snippet = if snippet.is_empty() {
                                "a file".into()
                            } else {
                                snippet
                            };
                            format!("↪ {}: {}", names(&person), snippet.replace('\n', " "))
                        }
                        _ => "↪ Earlier message".into(),
                    };
                    ui.label(ui::caption(line));
                }
                if start {
                    ui.horizontal(|ui| {
                        ui.label(ui::bold(&name));
                        ui.label(ui::caption(time(msg.at)));
                    });
                }
                match &msg.body {
                    Body::Text(text) => {
                        if !text.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                ui.add(Label::new(text.as_str()).wrap());
                                if chat.edited.contains_key(&msg.id) {
                                    ui.label(RichText::new("(edited)").small().weak());
                                }
                            });
                        }
                        attachments(ui);
                    }
                    _ => {
                        ui.label(
                            RichText::new("This message couldn't be opened.")
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
        Body::Text(text) => Some(text.clone()),
        _ => None,
    };
    let can_reply = text.is_some();
    let can_edit = mine && text.is_some();
    let can_delete = mine || can_moderate;
    let mut pick = None;
    ui::context_menu(&row.response, |m| {
        if can_reply && m.item("↩", "Reply").clicked() {
            pick = Some(Pick::Reply(msg.id));
        }
        if can_edit
            && let Some(text) = &text
            && m.item("✏", "Edit").clicked()
        {
            pick = Some(Pick::Edit(msg.id, text.clone()));
        }
        if let Some(text) = &text
            && m.item("📋", "Copy text").clicked()
        {
            m.ui().ctx().copy_text(text.clone());
        }
        if can_delete {
            m.separator();
            if m.danger("🗑", "Delete").clicked() {
                pick = Some(Pick::Delete(msg.id));
            }
        }
    });
    if !ui.rect_contains_pointer(rect) {
        return pick;
    }
    ui.painter().set(
        background,
        Shape::rect_filled(rect.expand(2.0), ui::RADIUS_S, ui::colors(ui).hover_soft),
    );
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
    if can_reply && ui::icon_button(&mut ui, "↩", "Reply", Look::Ghost, TOOLBAR_BUTTON).clicked()
    {
        pick = Some(Pick::Reply(msg.id));
    }
    if can_edit
        && let Some(text) = &text
        && ui::icon_button(&mut ui, "✏", "Edit", Look::Ghost, TOOLBAR_BUTTON).clicked()
    {
        pick = Some(Pick::Edit(msg.id, text.clone()));
    }
    if can_delete && ui::icon_button(&mut ui, "🗑", "Delete", Look::Ghost, TOOLBAR_BUTTON).clicked()
    {
        pick = Some(Pick::Delete(msg.id));
    }
    pick
}

fn dialog_title(home: &Home, dialog: &Dialog) -> String {
    match dialog {
        Dialog::ServerName(..) => "Server name".into(),
        Dialog::MyName(..) => "Your name here".into(),
        Dialog::Category(..) => "Create category".into(),
        Dialog::Channel { .. } => "Create channel".into(),
        Dialog::RenameChannel { .. } => "Rename channel".into(),
        Dialog::RenameCategory(..) => "Rename category".into(),
        Dialog::DeleteItem(_, reorder::Item::Channel(_), _) => "Delete channel?".into(),
        Dialog::DeleteItem(_, reorder::Item::Category(_), _) => "Delete category?".into(),
        Dialog::Invite(invite) => {
            format!("Invite people to {}", home.servers[invite.server].name())
        }
        Dialog::Member(index, person) => home.servers[*index].member_name(person),
        Dialog::Delete(..) => "Delete message?".into(),
        Dialog::Leave(index, _) => format!("Leave {}?", home.servers[*index].name()),
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
    let ((), dismissed) = ui::dialog(ctx, "dialog", &title, |ui| match dialog {
        Dialog::ServerName(index, name) => {
            ui.label("Everyone in the server sees this name.");
            if text_dialog(ui, name, "Save", &mut close) {
                command = Some((*index, Command::SetServerName(name.trim().to_owned())));
            }
        }
        Dialog::MyName(index, name) => {
            ui.label("People in this server see you by this name.");
            if text_dialog(ui, name, "Save", &mut close) {
                command = Some((*index, Command::SetDisplayName(name.trim().to_owned())));
            }
        }
        Dialog::Category(index, name) => {
            if text_dialog(ui, name, "Create", &mut close) {
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
                ui.selectable_value(voice, false, "# Text");
                ui.selectable_value(voice, true, "🔊 Voice");
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
                    .map_or("New category", |(_, name)| name.as_str());
                egui::ComboBox::from_label("Category")
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for (id, name) in &categories {
                            ui.selectable_value(category, Some(*id), name.as_str());
                        }
                        ui.selectable_value(category, None, "New category");
                    });
            }
            if category.is_none() {
                let label = ui.label("New category name");
                ui.add(ui::field(new_category).desired_width(f32::INFINITY))
                    .labelled_by(label.id);
            }
            ui.label("Name");
            if text_dialog(ui, name, "Create", &mut close)
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
        } => {
            let label = ui.label("Channel name").id;
            let name_field = name_field(ui, name).labelled_by(label);
            let label = ui.label("Topic (optional)").id;
            let field = ui
                .add(
                    ui::field(topic)
                        .char_limit(250)
                        .desired_width(f32::INFINITY),
                )
                .labelled_by(label);
            *topic_changed |= field.changed();
            let submitted = submitted(ui, &name_field) || submitted(ui, &field);
            if text_footer(ui, "Save", submitted, &mut close) && !name.trim().is_empty() {
                command = Some((
                    *server,
                    Command::RenameChannel {
                        id: *id,
                        name: name.trim().to_owned(),
                        topic: topic_changed.then(|| topic.trim().to_owned()),
                    },
                ));
            }
        }
        Dialog::RenameCategory(index, id, name) => {
            let label = ui.label("Category name").id;
            let field = name_field(ui, name).labelled_by(label);
            let submitted = submitted(ui, &field);
            if text_footer(ui, "Save", submitted, &mut close) && !name.trim().is_empty() {
                command = Some((*index, Command::RenameCategory(*id, name.trim().to_owned())));
            }
        }
        Dialog::DeleteItem(index, item, name) => {
            let (prompt, detail, delete) = match *item {
                reorder::Item::Channel(id) => (
                    format!("Delete “{name}” for everyone?"),
                    "This permanently deletes the channel and its message history.",
                    Command::DeleteChannel(id),
                ),
                reorder::Item::Category(id) => (
                    format!("Delete “{name}”?"),
                    "Only empty categories can be deleted. Move any channels to another category first.",
                    Command::DeleteCategory(id),
                ),
            };
            ui.label(prompt);
            ui.label(detail);
            ui::footer(ui, |ui| {
                if ui::danger(ui, "Delete").clicked() {
                    command = Some((*index, delete));
                }
                close |= ui::secondary(ui, "Cancel").clicked();
            });
        }
        Dialog::Invite(invite) => {
            let server = &home.servers[invite.server];
            match &invite.link {
                None => {
                    let uses = [(1, "1 person"), (10, "10 people"), (1000, "Unlimited")];
                    let times = [
                        (60 * 60, "1 hour"),
                        (DAY, "1 day"),
                        (DAY * 7, "7 days"),
                        (DAY * 30, "30 days"),
                    ];
                    ui.label(ui::bold("Who can use it"));
                    ui.horizontal(|ui| {
                        for (value, label) in uses {
                            ui.selectable_value(&mut invite.uses, value, label);
                        }
                    });
                    ui.label(ui::bold("Stops working after"));
                    ui.horizontal(|ui| {
                        for (value, label) in times {
                            ui.selectable_value(&mut invite.seconds, value, label);
                        }
                    });
                    ui::footer(ui, |ui| {
                        if invite.making {
                            ui.spinner();
                        } else if ui::primary(ui, "Create link").clicked() {
                            invite.making = true;
                            server.send(Command::Invite(invite.uses, invite.seconds));
                        }
                        close |= ui::secondary(ui, "Cancel").clicked();
                    });
                }
                Some((id, link)) => {
                    ui.label("Send this link to a friend.");
                    let mut shown = link.as_str();
                    ui.add(ui::field(&mut shown).desired_width(f32::INFINITY));
                    ui::footer(ui, |ui| {
                        if ui::primary(ui, "Copy link").clicked() {
                            ui.ctx().copy_text(link.clone());
                        }
                        if ui::secondary(ui, "Stop this link").clicked() {
                            command = Some((
                                invite.server,
                                Command::Admin(Admin::RevokeInvite(id.clone())),
                            ));
                        }
                        close |= ui::secondary(ui, "Done").clicked();
                    });
                }
            }
        }
        Dialog::Member(index, person) => {
            let server = &home.servers[*index];
            ui.label("Compare these with your friend to be sure it's really them.");
            ui.add_space(6.0);
            ui.label(format!("ID  {}", fingerprint(person)));
            if let Some(member) = server.dir.members.iter().find(|m| m.person == *person) {
                ui::section(ui, "Devices");
                for device in &member.devices {
                    ui.label(format!("{}  {}", device.name, device.fingerprint));
                }
            }
            ui::footer(ui, |ui| close |= ui::secondary(ui, "Close").clicked());
        }
        Dialog::Delete(index, channel, id) => {
            ui.label("It's removed for everyone. This can't be undone.");
            ui::footer(ui, |ui| {
                if ui::danger(ui, "Delete").clicked() {
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
                close |= ui::secondary(ui, "Cancel").clicked();
            });
        }
        Dialog::Leave(index, error) => {
            ui.label("It's removed from all your devices. To come back, you need a new invite.");
            error_text(ui, error.as_deref());
            ui::footer(ui, |ui| {
                if ui::danger(ui, "Leave").clicked() {
                    // Stays open until the server confirms; then the server is removed.
                    *error = None;
                    home.servers[*index].send(Command::Leave);
                }
                close |= ui::secondary(ui, "Cancel").clicked();
            });
        }
    });
    if let Some((index, command)) = command {
        home.servers[index].send(command);
        close = true;
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
        *close |= ui::secondary(ui, "Cancel").clicked();
    });
    done
}

#[cfg(test)]
mod tests {
    use super::*;

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
