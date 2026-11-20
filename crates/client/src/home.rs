//! The signed-in window: server rail, channels, chat and members.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use anyhow::Result;
use chrono::{DateTime, Local};
use eframe::egui::{
    self, Align, Align2, Button, CentralPanel, CollapsingHeader, Color32, FontId, Frame,
    Id as UiId, Key, KeyboardShortcut, Label, Layout, Margin, Modal, Modifiers, Panel, Popup, Rect,
    Response, RichText, Role, ScrollArea, Sense, Shape, TextEdit, Ui, UiBuilder, Vec2, WidgetInfo,
    ecolor::Hsva,
};
use rusttalk_core::{
    auth::InviteLink,
    client::{MAX_TEXT_CHARS, voice::LiveFileEvent},
    identity::{DeviceIdentity, fingerprint},
};

use crate::{
    admin,
    app::{Action, error_text, primary, secondary, submitted},
    audio::Audio,
    call::{self, LiveFile, LiveState, Voice},
    camera,
    device_sync::{self, Edit},
    files::{self, Ask, Files},
    net::{Admin, AdminUpdate, Admission, Command, Net, Preview, Update},
    screen::Video,
    server::{Body, Chat, Directory, Id, Msg, Server, Status, member_name},
};

/// The name shown until a server has one, or until its key arrives.
const NEW_SERVER: &str = "New server";
/// Show the character count once a message gets this long.
const COUNT_FROM: usize = 1_800;
const DAY: u32 = 24 * 60 * 60;

pub struct Home {
    pub identity: Arc<DeviceIdentity>,
    net: Net,
    pub servers: Vec<Server>,
    view: View,
    invite: String,
    join: Join,
    dialog: Option<Dialog>,
    pub toast: Option<(String, f64)>,
    pub settings: Option<Tab>,
    pub voice: Voice,
    pub files: Files,
    pub confirm_sign_out: bool,
    pub error: Option<String>,
    pub server_settings: Option<admin::Settings>,
    /// The synced voice and video settings as last published or received.
    synced: BTreeMap<String, serde_json::Value>,
    /// Servers left in this session, so a sync that started before can't bring them back.
    left: HashSet<url::Url>,
}

#[derive(Clone, Copy, PartialEq)]
enum View {
    Join,
    /// A DM: server index and channel.
    Direct(Option<(usize, Id)>),
    Server(usize),
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
    Invite(Invite),
    Member(usize, Id),
    Delete(usize, Id, Id),
    Leave(usize),
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
    Voice,
    Video,
    Devices,
}

impl Home {
    pub fn new(identity: DeviceIdentity, ctx: &egui::Context, data: PathBuf) -> Result<Self> {
        let identity = Arc::new(identity);
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
        Ok(Self {
            identity,
            net,
            view: if servers.is_empty() {
                View::Join
            } else {
                View::Server(0)
            },
            servers,
            invite: String::new(),
            join: Join::Idle(None),
            dialog: None,
            toast: None,
            settings: None,
            voice: Voice::new(audio, video, runtime),
            files: Files::default(),
            confirm_sign_out: false,
            error: None,
            server_settings: None,
            synced,
            left: HashSet::new(),
        })
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
                    View::Join
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
    fn publish_settings(&mut self) {
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
                        self.servers.remove(index);
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
                    server.dir = *dir;
                }
            }
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
            Update::NoFiles(address, channel) => {
                if let Some(call) = &mut self.voice.call
                    && call.server == address
                    && call.channel == channel
                {
                    call.can_send_files = false;
                }
            }
            Update::LiveOffer(offer) => self.voice.live_files.push(LiveFile {
                id: offer.id,
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
            Update::Failed(error) => {
                if let Some(Dialog::Invite(invite)) = &mut self.dialog {
                    invite.making = false;
                }
                self.toast = Some((error, now + 6.0));
            }
        }
    }
}

fn host(address: &url::Url) -> String {
    address.host_str().unwrap_or("server").to_owned()
}

fn initials(name: &str) -> String {
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

pub struct Palette {
    rail: Color32,
    pub side: Color32,
    main: Color32,
    hover: Color32,
    input: Color32,
}

pub fn palette(ui: &Ui) -> Palette {
    if ui.visuals().dark_mode {
        Palette {
            rail: Color32::from_rgb(0x1e, 0x1f, 0x22),
            side: Color32::from_rgb(0x2b, 0x2d, 0x31),
            main: Color32::from_rgb(0x31, 0x33, 0x38),
            hover: Color32::from_rgb(0x2e, 0x30, 0x35),
            input: Color32::from_rgb(0x38, 0x3a, 0x40),
        }
    } else {
        Palette {
            rail: Color32::from_rgb(0xe3, 0xe5, 0xe8),
            side: Color32::from_rgb(0xf2, 0xf3, 0xf5),
            main: Color32::WHITE,
            hover: Color32::from_rgb(0xf5, 0xf6, 0xf7),
            input: Color32::from_rgb(0xeb, 0xed, 0xef),
        }
    }
}

fn panel_frame(fill: Color32) -> Frame {
    Frame::new().fill(fill).inner_margin(Margin::same(8))
}

pub fn show(ui: &mut Ui, home: &mut Home) -> Option<Action> {
    let now = ui.input(|input| input.time);
    while let Ok(update) = home.net.updates.try_recv() {
        home.apply(ui.ctx(), update, now);
    }
    for (ask, paths) in home.files.answers() {
        let (Ask::Attach(address, _)
        | Ask::Save(address, _)
        | Ask::Offer(address)
        | Ask::Accept(address, ..)) = &ask;
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
            Ask::Offer(_) => {
                let Some(path) = paths.into_iter().next() else {
                    continue;
                };
                if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > call::MAX_LIVE_FILE) {
                    home.toast =
                        Some(("Files sent in a call can be up to 2 GB.".into(), now + 6.0));
                } else {
                    server.send(Command::OfferFile(path));
                }
            }
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
    let colors = palette(ui);
    if home.voice.watching.as_ref().is_some_and(|w| w.full) {
        CentralPanel::default()
            .frame(panel_frame(colors.main))
            .show(ui, |ui| call::screen_view(ui, home));
        call::frame(ui.ctx(), home);
        return None;
    }
    Panel::left("servers")
        .resizable(false)
        .exact_size(72.0)
        .frame(panel_frame(colors.rail))
        .show(ui, |ui| server_rail(ui, home));
    Panel::left("channels")
        .resizable(false)
        .exact_size(240.0)
        .frame(panel_frame(colors.side))
        .show(ui, |ui| sidebar(ui, home));
    if let View::Server(index) = home.view
        && home.servers[index].status != Status::Connecting
    {
        Panel::right("members")
            .resizable(false)
            .exact_size(220.0)
            .frame(panel_frame(colors.side))
            .show(ui, |ui| member_list(ui, home, index));
    }
    CentralPanel::default()
        .frame(
            Frame::new()
                .fill(colors.main)
                .inner_margin(Margin::same(12)),
        )
        .show(ui, |ui| match home.view {
            View::Join => join(ui, home),
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
    dialogs(ui.ctx(), home);
    admin::window(ui.ctx(), home);
    files::viewer(ui.ctx(), &mut home.files);
    call::stats_window(ui.ctx(), home);
    call::screen_dialog(ui.ctx(), home);
    call::frame(ui.ctx(), home);
    settings(ui.ctx(), home)
}

fn empty(ui: &mut Ui, title: &str, hint: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.heading(title);
        ui.weak(hint);
    });
}

fn rail_button(ui: &mut Ui, text: &str, label: &str, selected: bool) -> Response {
    let fill = if selected {
        ui.visuals().selection.bg_fill
    } else {
        palette(ui).main
    };
    let response = ui
        .add(
            Button::new(RichText::new(text).size(17.0).strong())
                .min_size(Vec2::splat(48.0))
                .corner_radius(if selected { 16 } else { 24 })
                .fill(fill)
                .stroke(egui::Stroke::NONE),
        )
        .on_hover_text(label);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    response
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
            if rail_button(ui, &initials(server.name()), server.name(), selected).clicked() {
                home.view = View::Server(index);
            }
        }
        if rail_button(ui, "+", "Add a server", home.view == View::Join).clicked() {
            home.view = View::Join;
        }
    });
}

fn sidebar(ui: &mut Ui, home: &mut Home) {
    Panel::bottom("you")
        .frame(Frame::new().inner_margin(Margin::symmetric(0, 6)))
        .show(ui, |ui| {
            call::call_bar(ui, home);
            if ui.button("⚙  Settings").clicked() {
                home.settings = Some(Tab::Profile);
            }
        });
    match home.view {
        View::Server(index) => channel_list(ui, home, index),
        _ => dm_list(ui, home),
    }
}

pub fn section(ui: &mut Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text.to_uppercase()).small().strong().weak());
}

fn dm_list(ui: &mut Ui, home: &mut Home) {
    ui.add_space(6.0);
    ui.label(RichText::new("Direct messages").strong().size(16.0));
    ui.separator();
    let mut any = false;
    ScrollArea::vertical().show(ui, |ui| {
        for (index, server) in home.servers.iter().enumerate() {
            for dm in &server.dir.dms {
                any = true;
                let selected = home.view == View::Direct(Some((index, dm.channel)));
                let name = server.member_name(&dm.peer);
                let row = ui
                    .horizontal(|ui| {
                        avatar(ui, &dm.peer, &name, 28.0);
                        ui.vertical(|ui| {
                            ui.add(
                                Button::selectable(selected, name.as_str())
                                    .frame_when_inactive(false),
                            )
                            .clicked()
                                | ui.label(RichText::new(server.name()).small().weak())
                                    .clicked()
                        })
                        .inner
                    })
                    .inner;
                if row {
                    home.view = View::Direct(Some((index, dm.channel)));
                }
            }
        }
    });
    if !any {
        ui.weak("No chats yet.");
    }
}

fn channel_list(ui: &mut Ui, home: &mut Home, index: usize) {
    let server = &home.servers[index];
    let perms = server.dir.perms;
    let online = server.status == Status::Online;
    ui.add_space(4.0);
    let header = ui.add(
        Button::new(
            RichText::new(format!("{}  ⏷", server.name()))
                .strong()
                .size(16.0),
        )
        .frame_when_inactive(false)
        .min_size(Vec2::new(ui.available_width(), 32.0)),
    );
    Popup::menu(&header).show(|ui| {
        ui.set_min_width(200.0);
        if perms.invite && ui.button("Invite people").clicked() {
            home.dialog = Some(Dialog::Invite(Invite {
                server: index,
                uses: 10,
                seconds: DAY * 7,
                making: false,
                link: None,
            }));
        }
        if perms.settings() && ui.button("Server settings").clicked() {
            admin::open(home, index, admin::Tab::Members);
        }
        if perms.manage_channels {
            if ui.button("Create channel").clicked() {
                home.dialog = Some(new_channel(&home.servers[index], index, None));
            }
            if ui.button("Create category").clicked() {
                home.dialog = Some(Dialog::Category(index, String::new()));
            }
        }
        if perms.owner && ui.button("Rename server").clicked() {
            let name = home.servers[index].dir.name.clone().unwrap_or_default();
            home.dialog = Some(Dialog::ServerName(index, name));
        }
        if ui.button("Change my name").clicked() {
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
        if ui.button("Leave server").clicked() {
            home.dialog = Some(Dialog::Leave(index));
        }
    });
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
            ui.colored_label(ui.visuals().error_fg_color, reason.as_str());
            if ui.button("Remove server").clicked() {
                home.leave_server(index);
                return;
            }
        }
        Status::Online => {}
    }
    if online && perms.owner && server.dir.name.is_none() {
        ui.add_space(6.0);
        if ui
            .add(Button::new("Name your server").fill(ui.visuals().selection.bg_fill))
            .clicked()
        {
            home.dialog = Some(Dialog::ServerName(index, String::new()));
        }
    }
    ui.separator();
    let mut pick = None;
    let mut add = None;
    let mut talk = None;
    let mut configure = None;
    ScrollArea::vertical().show(ui, |ui| {
        for category in &server.dir.categories {
            if !category.name.is_empty() {
                ui.horizontal(|ui| {
                    section(ui, &category.name);
                    if perms.manage_channels {
                        ui.with_layout(Layout::right_to_left(Align::Max), |ui| {
                            let plus = ui.small_button("+").on_hover_text("Create channel");
                            plus.widget_info(|| {
                                WidgetInfo::labeled(
                                    Role::Button,
                                    true,
                                    format!("Create channel in {}", category.name),
                                )
                            });
                            if plus.clicked() {
                                add = Some(category.id);
                            }
                        });
                    }
                });
            }
            for channel in &category.channels {
                let width = ui.available_width();
                if channel.voice {
                    let selected = server.open == Some(channel.id);
                    let button = Button::selectable(selected, format!("🔊  {}", channel.name))
                        .min_size(Vec2::new(width, 0.0));
                    let button = ui.add_enabled(online, button);
                    if button.clicked() {
                        talk = Some(channel.id);
                    }
                    if perms.manage_channels {
                        button.context_menu(|ui| {
                            if ui.button("Channel settings").clicked() {
                                configure = Some(channel.id);
                            }
                        });
                    }
                    call::room_list(ui, home, server, &channel.id);
                    continue;
                }
                let selected = server.open == Some(channel.id);
                let button = Button::selectable(selected, format!("#  {}", channel.name))
                    .min_size(Vec2::new(width, 0.0));
                let button = ui.add(button);
                if button.clicked() {
                    pick = Some(channel.id);
                }
                if perms.manage_channels {
                    button.context_menu(|ui| {
                        if ui.button("Channel settings").clicked() {
                            configure = Some(channel.id);
                        }
                    });
                }
            }
        }
    });
    if let Some(channel) = configure {
        admin::open(home, index, admin::Tab::Channel(channel));
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
        FontId::proportional(size * 0.38),
        Color32::WHITE,
    );
    response
}

fn member_list(ui: &mut Ui, home: &mut Home, index: usize) {
    let server = &home.servers[index];
    ui.add_space(6.0);
    section(ui, &format!("Members — {}", server.dir.members.len()));
    let perms = server.dir.perms;
    let mut action = None;
    let (mut manage, mut moderate) = (None, None);
    ScrollArea::vertical().show(ui, |ui| {
        for member in &server.dir.members {
            let name = server.member_name(&member.person);
            let me = member.person == server.dir.me;
            let row = ui
                .horizontal(|ui| {
                    avatar(ui, &member.person, &name, 30.0);
                    let label = if me {
                        format!("{name} (you)")
                    } else {
                        name.clone()
                    };
                    ui.add(
                        Button::selectable(false, label)
                            .right_text("")
                            .min_size(Vec2::new(ui.available_width(), 30.0)),
                    )
                })
                .inner;
            Popup::menu(&row).show(|ui| {
                if !me && ui.button("Message").clicked() {
                    action = Some(Command::OpenDm(member.person));
                }
                if ui.button("Details").clicked() {
                    home.dialog = Some(Dialog::Member(index, member.person));
                }
                if !me && home.voice.call.is_some() {
                    ui.separator();
                    call::stream_menu(ui, &home.voice.audio, &(member.person, None));
                }
                if !me && (perms.kick || perms.ban || perms.manage_roles) {
                    ui.separator();
                    manage = Some(member.person);
                    admin::member_menu(ui, &mut moderate, perms);
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
                ui.heading(if home.servers.is_empty() {
                    "No servers yet"
                } else {
                    "Join a server"
                });
                ui.label("Paste an invite link from a friend.");
                ui.add_space(12.0);
                let label = ui.label("Invite link");
                let field = ui
                    .add(
                        TextEdit::singleline(&mut home.invite)
                            .hint_text("rusttalk://…")
                            .desired_width(400.0),
                    )
                    .labelled_by(label.id);
                error_text(ui, error.as_deref());
                if primary(ui, "Join a server").clicked() || submitted(ui, &field) {
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
            }
            Join::Checking => {
                ui.spinner();
                ui.label("Checking the invite…");
            }
            Join::Joining(_) => {
                ui.spinner();
                ui.label("Joining…");
            }
            Join::Preview { preview, name } => {
                let address = preview.server.address.clone();
                if let Some(index) = home.servers.iter().position(|s| s.address == address) {
                    ui.heading("You're already in this server");
                    if primary(ui, "Open it").clicked() {
                        home.view = View::Server(index);
                        home.join = Join::Idle(None);
                    }
                    return;
                }
                match &preview.name {
                    Some(server) => ui.heading(format!("Join {server}?")),
                    None => ui.heading("Join this server?"),
                };
                ui.weak(host(&address));
                ui.add_space(12.0);
                let label = ui.label("Your name on this server");
                let field = ui
                    .add(
                        TextEdit::singleline(name)
                            .hint_text("What friends call you")
                            .desired_width(300.0),
                    )
                    .labelled_by(label.id);
                ui.add_space(8.0);
                if preview.invite.is_none() && !preview.open {
                    ui.label("This server needs an invite. Ask someone there for a link.");
                    if secondary(ui, "Back").clicked() {
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
                if primary(ui, "Join").clicked() || submitted(ui, &field) {
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
                } else if secondary(ui, "Cancel").clicked() {
                    home.join = Join::Idle(None);
                }
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
    let Home {
        servers,
        toast,
        dialog,
        files,
        ..
    } = home;
    let Server {
        address,
        chats,
        dir,
        status,
        commands,
        ..
    } = &mut servers[index];
    let chat = chats.entry(channel).or_insert_with(|| {
        // First look at this channel: load it, then follow live updates.
        let _ = commands.send(Command::Open(channel));
        let mut chat = Chat::default();
        chat.loading = true;
        chat
    });
    Panel::top("chat-title")
        .frame(Frame::new().inner_margin(Margin::symmetric(4, 6)))
        .show(ui, |ui| {
            ui.label(RichText::new(&title).strong().size(17.0));
        });
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
    if attach {
        files.ask(ui.ctx(), Ask::Attach(address.clone(), channel));
    }
    if online && hovered {
        let rect = ui.max_rect();
        ui.painter().rect_filled(
            rect,
            8.0,
            ui.visuals().selection.bg_fill.gamma_multiply(0.35),
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            format!("Drop to send in {title}"),
            FontId::proportional(20.0),
            ui.visuals().strong_text_color(),
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
    let output = area.show(ui, |ui| {
        ui.set_width(ui.available_width());
        if chat.at_start {
            ui.add_space(12.0);
            ui.label(
                RichText::new(format!("This is the start of {title}"))
                    .strong()
                    .size(18.0),
            );
            ui.add_space(12.0);
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
            ui.colored_label(ui.visuals().error_fg_color, text.as_str());
            close = ui.small_button("×").clicked();
        });
        if close {
            *toast = None;
        }
    }
}

/// Returns a message to send, and whether "Attach files" was clicked.
fn composer(
    ui: &mut Ui,
    chat: &mut Chat,
    dir: &Directory,
    online: bool,
    toast: &mut Option<(String, f64)>,
    channel: Id,
    title: &str,
) -> (Option<Command>, bool) {
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
            cancel = ui.small_button("×").on_hover_text("Cancel reply").clicked();
        });
    }
    if chat.editing.is_some() {
        ui.horizontal(|ui| {
            ui.weak("Editing message");
            cancel = ui.small_button("×").on_hover_text("Cancel edit").clicked();
        });
    }
    if let Some(what) = &chat.uploading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak(format!("Uploading {what}…"));
        });
    }
    files::staged(ui, chat);
    let input = palette(ui).input;
    let mut attach = false;
    let field = ui
        .horizontal_top(|ui| {
            let plus = ui
                .add_enabled(
                    online && chat.editing.is_none(),
                    Button::new(RichText::new("➕").size(16.0)).min_size(Vec2::splat(34.0)),
                )
                .on_hover_text("Attach files (up to 32 MB each)");
            plus.widget_info(|| WidgetInfo::labeled(Role::Button, plus.enabled(), "Attach files"));
            attach = plus.clicked();
            ScrollArea::vertical()
                .id_salt("composer-scroll")
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.add_enabled(
                        online,
                        TextEdit::multiline(&mut chat.draft)
                            .background_color(input)
                            .margin(Margin::symmetric(10, 8))
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
    let (enter, escape) = ui.input(|input| {
        (
            input.key_pressed(Key::Enter) && !input.modifiers.shift,
            input.key_pressed(Key::Escape),
        )
    });
    let focused = field.has_focus() || field.lost_focus();
    if cancel || (focused && escape) {
        chat.reply_to = None;
        if chat.editing.take().is_some() {
            chat.draft.clear();
        }
    }
    let count = chat.draft.chars().count();
    if count >= COUNT_FROM {
        let color = if count > MAX_TEXT_CHARS {
            ui.visuals().error_fg_color
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
    let command = Command::Send {
        channel,
        text: text.to_owned(),
        reply_to: chat.reply_to.take(),
        edit_of: chat.editing.take(),
        files,
    };
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
    let row = ui.horizontal_top(|ui| {
        if start {
            avatar(ui, &msg.person, &name, 38.0);
        } else {
            ui.add_space(38.0);
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
                ui.label(RichText::new(line).small().weak());
            }
            if start {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&name).strong());
                    ui.label(RichText::new(time(msg.at)).small().weak());
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
    let rect = row.response.rect;
    if !ui.rect_contains_pointer(rect) {
        return None;
    }
    ui.painter().set(
        background,
        Shape::rect_filled(rect.expand(2.0), 4.0, palette(ui).hover),
    );
    let mine = msg.person == me;
    let text = match &msg.body {
        Body::Text(text) => Some(text.clone()),
        _ => None,
    };
    let bar = Rect::from_min_size(
        rect.right_top() + Vec2::new(-170.0, -4.0),
        Vec2::new(170.0, 24.0),
    );
    let mut pick = None;
    // A child that takes no space, so the toolbar floats over the row.
    let mut ui = ui.new_child(
        UiBuilder::new()
            .max_rect(bar)
            .layout(Layout::right_to_left(Align::Min)),
    );
    {
        let ui = &mut ui;
        {
            if (mine || can_moderate) && ui.small_button("Delete").clicked() {
                pick = Some(Pick::Delete(msg.id));
            }
            if mine
                && let Some(text) = &text
                && ui.small_button("Edit").clicked()
            {
                pick = Some(Pick::Edit(msg.id, text.clone()));
            }
            if text.is_some() && ui.small_button("Reply").clicked() {
                pick = Some(Pick::Reply(msg.id));
            }
        }
    }
    pick
}

fn dialogs(ctx: &egui::Context, home: &mut Home) {
    let Some(dialog) = &mut home.dialog else {
        return;
    };
    let mut close = false;
    let mut command = None;
    let mut leave = None;
    let modal = Modal::new(UiId::unique("dialog")).show(ctx, |ui| {
        ui.set_width(400.0);
        match dialog {
            Dialog::ServerName(index, name) => {
                ui.heading("Server name");
                ui.label("Everyone in the server sees this name.");
                if text_dialog(ui, name, "Save", &mut close) {
                    command = Some((*index, Command::SetServerName(name.trim().to_owned())));
                }
            }
            Dialog::MyName(index, name) => {
                ui.heading("Your name here");
                ui.label("People in this server see you by this name.");
                if text_dialog(ui, name, "Save", &mut close) {
                    command = Some((*index, Command::SetDisplayName(name.trim().to_owned())));
                }
            }
            Dialog::Category(index, name) => {
                ui.heading("Create category");
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
                ui.heading("Create channel");
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
                    ui.text_edit_singleline(new_category).labelled_by(label.id);
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
            Dialog::Invite(invite) => {
                let server = &home.servers[invite.server];
                ui.heading(format!("Invite people to {}", server.name()));
                match &invite.link {
                    None => {
                        let uses = [(1, "1 person"), (10, "10 people"), (1000, "Unlimited")];
                        let times = [
                            (60 * 60, "1 hour"),
                            (DAY, "1 day"),
                            (DAY * 7, "7 days"),
                            (DAY * 30, "30 days"),
                        ];
                        ui.label("Who can use it");
                        ui.horizontal(|ui| {
                            for (value, label) in uses {
                                ui.selectable_value(&mut invite.uses, value, label);
                            }
                        });
                        ui.label("Stops working after");
                        ui.horizontal(|ui| {
                            for (value, label) in times {
                                ui.selectable_value(&mut invite.seconds, value, label);
                            }
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if invite.making {
                                ui.spinner();
                            } else if primary_small(ui, "Create link").clicked() {
                                invite.making = true;
                                server.send(Command::Invite(invite.uses, invite.seconds));
                            }
                            close |= ui.button("Cancel").clicked();
                        });
                    }
                    Some((id, link)) => {
                        ui.label("Send this link to a friend.");
                        let mut shown = link.as_str();
                        ui.add(TextEdit::singleline(&mut shown).desired_width(f32::INFINITY));
                        ui.horizontal(|ui| {
                            if primary_small(ui, "Copy link").clicked() {
                                ui.ctx().copy_text(link.clone());
                            }
                            if ui.button("Stop this link").clicked() {
                                command = Some((
                                    invite.server,
                                    Command::Admin(Admin::RevokeInvite(id.clone())),
                                ));
                            }
                            close |= ui.button("Done").clicked();
                        });
                    }
                }
            }
            Dialog::Member(index, person) => {
                let server = &home.servers[*index];
                ui.heading(server.member_name(person));
                ui.label("Compare these with your friend to be sure it's really them.");
                ui.add_space(6.0);
                ui.label(format!("ID  {}", fingerprint(person)));
                if let Some(member) = server.dir.members.iter().find(|m| m.person == *person) {
                    section(ui, "Devices");
                    for device in &member.devices {
                        ui.label(format!("{}  {}", device.name, device.fingerprint));
                    }
                }
                ui.add_space(6.0);
                close |= ui.button("Close").clicked();
            }
            Dialog::Delete(index, channel, id) => {
                ui.heading("Delete message?");
                ui.label("It's removed for everyone. This can't be undone.");
                ui.horizontal(|ui| {
                    if ui
                        .add(Button::new("Delete").fill(ui.visuals().error_fg_color))
                        .clicked()
                    {
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
                    close |= ui.button("Cancel").clicked();
                });
            }
            Dialog::Leave(index) => {
                ui.heading(format!("Leave {}?", home.servers[*index].name()));
                ui.label(
                    "It's removed from all your devices. To come back, you need a new invite.",
                );
                ui.horizontal(|ui| {
                    if ui
                        .add(Button::new("Leave").fill(ui.visuals().error_fg_color))
                        .clicked()
                    {
                        leave = Some(*index);
                    }
                    close |= ui.button("Cancel").clicked();
                });
            }
        }
    });
    if let Some(index) = leave {
        home.leave_server(index);
        return;
    }
    if let Some((index, command)) = command {
        home.servers[index].send(command);
        close = true;
    }
    if close || modal.should_close() {
        home.dialog = None;
    }
}

fn primary_small(ui: &mut Ui, text: &str) -> Response {
    ui.add(Button::new(text).fill(ui.visuals().selection.bg_fill))
}

/// A name field with a confirm button; returns true when confirmed with a non-empty name.
fn text_dialog(ui: &mut Ui, text: &mut String, confirm: &str, close: &mut bool) -> bool {
    let field = ui.add(
        TextEdit::singleline(text)
            .char_limit(100)
            .desired_width(f32::INFINITY),
    );
    if ui.memory(|memory| memory.focused().is_none()) {
        field.request_focus();
    }
    let mut done = submitted(ui, &field);
    ui.horizontal(|ui| {
        done |= primary_small(ui, confirm).clicked();
        *close |= ui.button("Cancel").clicked();
    });
    done && !text.trim().is_empty()
}

fn settings(ctx: &egui::Context, home: &mut Home) -> Option<Action> {
    let mut tab = home.settings?;
    let mut close = false;
    let modal = Modal::new(UiId::unique("settings")).show(ctx, |ui| {
        ui.set_width(440.0);
        ui.heading("Settings");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut tab, Tab::Profile, "Profile");
            ui.selectable_value(&mut tab, Tab::Voice, "Voice");
            ui.selectable_value(&mut tab, Tab::Video, "Video");
            ui.selectable_value(&mut tab, Tab::Devices, "Devices");
        });
        ui.separator();
        match tab {
            Tab::Profile => {
                ui.label("You pick your name on each server. Change it from the server's menu.");
                CollapsingHeader::new("Details").show(ui, |ui| {
                    ui.label("Your ID");
                    ui.monospace(fingerprint(home.identity.certificate.person_id()));
                    ui.weak("Friends can compare this to make sure it's really you.");
                });
            }
            Tab::Voice => call::settings_tab(ui, &mut home.voice),
            Tab::Video => {
                let audio = home.voice.audio.clone();
                let mut s = audio.settings();
                camera::settings_tab(ui, &mut home.voice.camera, &mut s);
                audio.set_settings(s);
            }
            Tab::Devices => {
                ui.label(format!("This device: {}", home.identity.certificate.name()));
            }
        }
        error_text(ui, home.error.as_deref());
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Sign out of this device").clicked() {
                home.confirm_sign_out = true;
            }
            close = ui.button("Close").clicked();
        });
    });
    home.settings = Some(tab);
    if close || (modal.should_close() && !home.confirm_sign_out) {
        home.settings = None;
        home.error = None;
        home.publish_settings();
    }

    if !home.confirm_sign_out {
        return None;
    }
    let mut action = None;
    let mut cancel = false;
    let modal = Modal::new(UiId::unique("sign-out")).show(ctx, |ui| {
        ui.set_width(380.0);
        ui.heading("Sign out of this device?");
        ui.label("This removes your account from this computer. To sign in again, you need your 24 recovery words.");
        ui.horizontal(|ui| {
            if ui.button("Sign out").clicked() {
                action = Some(Action::SignOut);
            }
            cancel = ui.button("Cancel").clicked();
        });
    });
    if cancel || modal.should_close() {
        home.confirm_sign_out = false;
    }
    action
}
