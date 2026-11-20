//! Network work for joined servers. It runs on a tokio runtime off the UI thread:
//! the UI sends each server task a `Command` and drains `Update`s every frame.
//! Never log names, message text or invite links here.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use eframe::egui;
use livekit::prelude::RemoteTrack;
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::{
        InviteCode, InviteLink, KnownServer, ProtocolError, Session, decode, encode, voice_identity,
    },
    client::{
        Catalog, Event, KeyClient, MessageBody, MessageError, MessageRecord, Server as Api,
        invite_name, save_json,
        voice::{
            ConnectionState, RoomEvent, TrackSid, TrackSource, VoiceRoom, VoiceRoster, VoiceState,
        },
    },
    identity::{DeviceIdentity, fingerprint},
    keys::Metadata,
    pinned_tls::PinnedElsewhere,
};
use serde_json::Value;
use tokio::{
    runtime::Runtime,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};
use url::Url;

use crate::{
    audio::Audio,
    screen::{self, Video},
    server::{
        Body, CallState, Category, Channel, Device, Directory, Dm, Id, Member, Msg, Peer, Perms,
        Screen, Stats, Status, TrackStats, member_name,
    },
    share::{self, App, Capture},
};

const PAGE: u32 = 50;
const CHANGED: &str = "This server changed. Don't continue unless the owner told you it would.";
const OFFLINE: &str = "You're offline. Try again in a moment.";
const FAILED: &str = "That didn't work. Try again.";
/// Invites made from the UI.
const INVITE_USES: u32 = 10;
const INVITE_SECONDS: u32 = 7 * 24 * 60 * 60;

pub enum Command {
    /// Load the newest page and follow live messages.
    Open(Id),
    /// Load the page before a message: channel, oldest loaded message.
    Older(Id, Id),
    Send {
        channel: Id,
        text: String,
        reply_to: Option<Id>,
        edit_of: Option<Id>,
    },
    Delete(Id, Id),
    CreateChannel {
        /// `None` creates a category named `new_category` first.
        category: Option<Id>,
        new_category: String,
        voice: bool,
        name: String,
    },
    CreateCategory(String),
    SetServerName(String),
    SetDisplayName(String),
    OpenDm(Id),
    Invite,
    JoinVoice(Id),
    LeaveVoice,
    ShareApp(App),
    /// Stop sharing an app, by its ID.
    StopApp(String),
    /// Share a screen or window, or open the system picker (Linux).
    ShareScreen(screen::Target, screen::Options),
    StopScreen,
    /// Start or stop receiving a screen share, by track.
    Watch(String, bool),
}

pub struct Preview {
    pub link: InviteLink,
    /// The server name from the invite, when the invite carries one.
    pub name: Option<String>,
    /// This profile has connected to this address before.
    pub known: bool,
}

pub enum Update {
    Preview(Result<Box<Preview>, String>),
    Status(Url, Status),
    Directory(Url, Box<Directory>),
    Messages {
        server: Url,
        channel: Id,
        items: Vec<Msg>,
        older: bool,
        /// Set for page loads: whether this was the first page of the channel.
        at_start: Option<bool>,
    },
    Deleted(Url, Id, Id),
    Notice(Url, Id, Msg),
    DmReady(Url, Id),
    Invite(Url, String),
    /// The call in a voice channel changed state; `None` when it ended.
    Call(Url, Id, Option<CallState>),
    /// This device joined the call without permission to talk.
    Listening(Url, Id),
    /// This device may not share its screen in the call.
    NoScreen(Url, Id),
    /// Everyone in one voice channel.
    Room(Url, Id, Vec<Peer>),
    /// Sharing this app stopped or never started.
    AppStopped(String),
    /// This device's screen share went live (`true`), or stopped or never started.
    Sharing(bool),
    Stats(Url, Stats),
    Failed(String),
}

pub struct Net {
    runtime: Option<Runtime>,
    ctx: Ctx,
    pub updates: mpsc::Receiver<Update>,
}

#[derive(Clone)]
struct Ctx {
    updates: mpsc::Sender<Update>,
    egui: egui::Context,
    identity: Arc<DeviceIdentity>,
    data: Arc<Path>,
    audio: Audio,
    video: Video,
}

impl Ctx {
    fn send(&self, update: Update) {
        if self.updates.send(update).is_ok() {
            self.egui.request_repaint();
        }
    }
}

impl Net {
    pub fn new(
        egui: egui::Context,
        identity: Arc<DeviceIdentity>,
        data: PathBuf,
        audio: Audio,
        video: Video,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rusttalk-net")
            .enable_all()
            .build()
            .context("start network runtime")?;
        let (updates, receiver) = mpsc::channel();
        Ok(Self {
            runtime: Some(runtime),
            ctx: Ctx {
                updates,
                egui,
                identity,
                data: data.into(),
                audio,
                video,
            },
            updates: receiver,
        })
    }

    /// Servers this profile joined earlier, to reconnect at start.
    pub fn joined(&self) -> Vec<KnownServer> {
        load_joined(&self.ctx.data).unwrap_or_else(|error| {
            tracing::error!(error = %format_args!("{error:#}"), "reading joined servers failed");
            Vec::new()
        })
    }

    /// Check an invite's server and read its name, without joining yet.
    pub fn preview(&self, link: InviteLink) {
        let ctx = self.ctx.clone();
        self.spawn(async move {
            let result = preview(&ctx, &link).await;
            let update = result.map(|(name, known)| Box::new(Preview { link, name, known }));
            ctx.send(Update::Preview(update));
        });
    }

    /// Connect to a server (signing in with `invite` the first time) and keep it connected.
    pub fn start(
        &self,
        server: KnownServer,
        invite: Option<InviteCode>,
        display_name: Option<String>,
    ) -> UnboundedSender<Command> {
        let (commands, receiver) = unbounded_channel();
        let ctx = self.ctx.clone();
        self.spawn(run(ctx, server, invite, display_name, receiver));
        commands
    }

    /// Drop a server from the saved list.
    pub fn forget(&self, address: &Url) {
        if let Err(error) = edit_joined(&self.ctx.data, |list| {
            list.retain(|s| &s.address != address)
        }) {
            tracing::error!(error = %format_args!("{error:#}"), "removing a server failed");
        }
    }

    fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        if let Some(runtime) = &self.runtime {
            runtime.spawn(task);
        }
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        // Never wait for network tasks on the UI thread.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

async fn preview(ctx: &Ctx, link: &InviteLink) -> Result<(Option<String>, bool), String> {
    let known = Api::pinned(&ctx.data, &link.server.address).map_err(|error| {
        tracing::error!(error = %format_args!("{error:#}"), "reading server pins failed");
        "Couldn't read your saved servers.".to_owned()
    })?;
    let trusted = known.as_ref().unwrap_or(&link.server);
    let api = Api::probe(
        link.server.address.clone(),
        link.server.tls_fingerprint,
        Some(trusted),
    )
    .await
    .map_err(|error| explain(&error, known.is_some()))?;
    if api.known_server() != &link.server {
        tracing::warn!("invite pins differ from the server");
        return Err(mismatch(known.is_some()));
    }
    let name = match &link.link_key {
        // Owner invites from the server console carry no name.
        None => None,
        Some(_) => invite_name(&api, link)
            .await
            .map_err(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "invite preview failed");
                "This invite has expired or was already used. Ask for a new one.".to_owned()
            })?
            .map(|name| String::from_utf8_lossy(&name).into_owned()),
    };
    tracing::info!(server = %fingerprint(&link.server.server_id), known = known.is_some(), "invite checked");
    Ok((name, known.is_some()))
}

fn explain(error: &anyhow::Error, known: bool) -> String {
    if pin_changed(error) {
        tracing::error!(error = %format_args!("{error:#}"), "server pins differ");
        mismatch(known)
    } else {
        tracing::warn!(error = %format_args!("{error:#}"), "server unreachable");
        "Can't reach this server. Check the link and your connection.".into()
    }
}

fn mismatch(known: bool) -> String {
    if known {
        CHANGED.into()
    } else {
        "This invite doesn't match the server. Ask for a new invite.".into()
    }
}

fn pin_changed(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ProtocolError>(),
        Some(ProtocolError::ServerIdChanged | ProtocolError::TlsChanged | ProtocolError::ServerProof)
    ) ||
        // The pinned TLS verifier reports another certificate only inside the transport error text.
        format!("{error:#}").contains("fingerprint mismatch")
}

#[derive(Default)]
struct State {
    /// Channels the UI has opened, which get live messages.
    loaded: HashSet<Id>,
    /// Records that waited for a channel key, retried after key work.
    waiting: HashMap<Id, HashSet<Id>>,
    last: Option<Directory>,
    /// Whether `last` was built with the space key.
    named: bool,
    display_name: Option<String>,
    notices: u64,
    server_id: Id,
    /// Who the server says is in each voice channel.
    rooms: HashMap<Id, VoiceRoster>,
    voice: Option<Live>,
}

/// The voice room this device is in, on this server.
struct Live {
    room: VoiceRoom,
    channel: Id,
    /// Apps this device shares, each with its own track.
    shares: Vec<Share>,
    /// Decrypted names of subscribed remote app tracks.
    apps: HashMap<TrackSid, String>,
    /// A capture waiting for the person to pick (Linux) or for its first frame.
    starting: Option<tokio::task::JoinHandle<Result<(screen::Capture, screen::Options)>>>,
    screen: Option<(TrackSid, screen::Capture)>,
    /// Screen tracks this device receives; others are unsubscribed.
    watched: HashSet<String>,
}

struct Share {
    app: App,
    sid: TrackSid,
    /// Stops capturing when dropped.
    capture: Capture,
}

enum Failure {
    Retry,
    Stop(String),
}

enum End {
    Quit,
    Closed(Option<String>),
    Error,
}

async fn run(
    ctx: Ctx,
    server: KnownServer,
    mut invite: Option<InviteCode>,
    display_name: Option<String>,
    mut commands: UnboundedReceiver<Command>,
) {
    let url = server.address.clone();
    let mut state = State {
        display_name,
        server_id: server.server_id,
        rooms: HashMap::new(),
        voice: None,
        ..State::default()
    };
    let mut delay = 1;
    // Verified once, then reused: each verification spends one of the server's rate-limited challenges.
    let mut api = None;
    loop {
        ctx.send(Update::Status(url.clone(), Status::Connecting));
        match session(&ctx, &server, &mut api, invite.as_ref()).await {
            Ok((api, session)) => {
                if invite.take().is_some() {
                    tracing::info!(server = %fingerprint(&server.server_id), "joined server");
                    if let Err(error) = edit_joined(&ctx.data, |list| {
                        list.retain(|s| s.address != server.address);
                        list.push(server.clone());
                    }) {
                        tracing::error!(error = %format_args!("{error:#}"), "saving joined server failed");
                    }
                }
                match KeyClient::connect(api, &ctx.identity, &session).await {
                    Ok(mut client) => {
                        delay = 1;
                        tracing::info!(server = %fingerprint(&server.server_id), "connected to server");
                        ctx.send(Update::Status(url.clone(), Status::Online));
                        match serve(&ctx, &url, &mut client, &mut state, &mut commands).await {
                            End::Quit => return,
                            End::Closed(Some(reason))
                                if matches!(reason.as_str(), "kicked" | "banned" | "revoked") =>
                            {
                                tracing::info!(%reason, "server ended membership");
                                let text = match reason.as_str() {
                                    "kicked" => "You were removed from this server.",
                                    "banned" => "You're banned from this server.",
                                    _ => "This device was removed from your account.",
                                };
                                ctx.send(Update::Status(url, Status::Stopped(text.into())));
                                return;
                            }
                            End::Closed(reason) => {
                                tracing::info!(
                                    reason = reason.as_deref().unwrap_or("none"),
                                    "event stream closed"
                                );
                            }
                            End::Error => {}
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %format_args!("{error:#}"), "opening the event stream failed")
                    }
                }
            }
            Err(Failure::Stop(text)) => {
                ctx.send(Update::Status(url, Status::Stopped(text)));
                return;
            }
            Err(Failure::Retry) => {}
        }
        ctx.send(Update::Status(url.clone(), Status::Offline));
        let wait = tokio::time::sleep(Duration::from_secs(delay));
        tokio::pin!(wait);
        loop {
            tokio::select! {
                () = &mut wait => break,
                command = commands.recv() => match command {
                    None => return,
                    Some(Command::LeaveVoice) => {
                        if let Some(live) = state.voice.take() {
                            tokio::spawn(leave_room(live.room));
                        }
                    }
                    // The voice room outlives the API connection, so its shares can still stop.
                    Some(Command::StopApp(id)) => {
                        if let Some(live) = &mut state.voice
                            && let Some(at) = live.shares.iter().position(|share| share.app.id == id)
                        {
                            let share = live.shares.remove(at);
                            tracing::info!("stopped sharing app audio");
                            if let Err(error) = live.room.unpublish_audio(&share.sid).await {
                                tracing::warn!(error = %format_args!("{error:#}"), "stopping app audio failed");
                            }
                        }
                    }
                    Some(Command::ShareApp(app)) => {
                        ctx.send(Update::AppStopped(app.id));
                        ctx.send(Update::Failed(OFFLINE.into()));
                    }
                    Some(Command::StopScreen) => {
                        if let Some(live) = &mut state.voice {
                            if let Some(starting) = live.starting.take() {
                                starting.abort();
                            }
                            if let Some((sid, _capture)) = live.screen.take() {
                                tracing::info!("stopped sharing screen");
                                if let Err(error) = live.room.unpublish_screen(&sid).await {
                                    tracing::warn!(error = %format_args!("{error:#}"), "stopping screen failed");
                                }
                            }
                        }
                    }
                    Some(Command::ShareScreen(..)) => {
                        ctx.send(Update::Sharing(false));
                        ctx.send(Update::Failed(OFFLINE.into()));
                    }
                    Some(Command::Watch(sid, on)) => {
                        if let Some(live) = &mut state.voice
                            && let Err(error) = watch(&ctx, live, sid, on)
                        {
                            tracing::warn!(error = %format_args!("{error:#}"), "watching a screen failed");
                        }
                    }
                    Some(command) => {
                        if let Command::JoinVoice(channel) = command {
                            ctx.send(Update::Call(url.clone(), channel, None));
                        }
                        ctx.send(Update::Failed(OFFLINE.into()));
                    }
                },
            }
        }
        delay = (delay * 2).min(30);
    }
}

async fn session<'a>(
    ctx: &Ctx,
    server: &KnownServer,
    api: &'a mut Option<Api>,
    invite: Option<&InviteCode>,
) -> Result<(&'a Api, Session), Failure> {
    let stop_or_retry = |error: anyhow::Error| {
        if pin_changed(&error) {
            tracing::error!(error = %format_args!("{error:#}"), "server pins differ");
            Failure::Stop(CHANGED.into())
        } else {
            tracing::warn!(error = %format_args!("{error:#}"), "server unreachable");
            Failure::Retry
        }
    };
    if api.is_none() {
        let verified = Api::connect(
            server.address.clone(),
            server.tls_fingerprint,
            Some(server),
            &ctx.data,
        )
        .await
        .map_err(stop_or_retry)?;
        *api = Some(verified);
    }
    let api = api.as_ref().expect("set above");
    let reply = api
        .sign_in(&ctx.identity, invite)
        .await
        .map_err(stop_or_retry)?;
    let status = reply.status();
    if status.is_success() {
        let session = reply
            .json()
            .await
            .map_err(|error| stop_or_retry(error.without_url().into()))?;
        return Ok((api, session));
    }
    let code = reply
        .json::<Value>()
        .await
        .ok()
        .and_then(|body| body["error"].as_str().map(str::to_owned))
        .unwrap_or_default();
    tracing::warn!(%status, code, "sign-in refused");
    Err(match code.as_str() {
        "invite_required" if invite.is_none() => {
            Failure::Stop("You're no longer a member of this server.".into())
        }
        "invite_required" | "invalid_invite" | "invite_not_found" => {
            Failure::Stop("This invite has expired or was already used. Ask for a new one.".into())
        }
        "person_banned" | "ip_banned" => Failure::Stop("You're banned from this server.".into()),
        "device_revoked" => Failure::Stop("This device was removed from your account.".into()),
        "device_limit" => Failure::Stop(
            "Your account already has 10 devices on this server. Remove one first.".into(),
        ),
        _ if status.is_server_error()
            || status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::UNAUTHORIZED =>
        {
            Failure::Retry
        }
        _ => Failure::Stop("Couldn't sign in to this server.".into()),
    })
}

async fn serve(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    commands: &mut UnboundedReceiver<Command>,
) -> End {
    if let Err(error) = resume(ctx, url, client, state).await {
        tracing::warn!(error = %format_args!("{error:#}"), "loading server state failed");
        return End::Error;
    }
    let mut stats = tokio::time::interval(Duration::from_secs(2));
    loop {
        // Refresh ten minutes before the 12-hour token expires.
        let refresh = Duration::from_secs((client.expires_at() - 600 - now()).max(0) as u64);
        tokio::select! {
            event = client.next_event() => match event {
                Ok(Some(event)) => {
                    if let Err(error) = on_event(ctx, url, client, state, event).await {
                        tracing::warn!(error = %format_args!("{error:#}"), "handling a server event failed");
                    }
                    sync_voice_key(client, state);
                }
                Ok(None) => return End::Closed(client.close_reason.clone()),
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "event stream failed");
                    return End::Error;
                }
            },
            command = commands.recv() => {
                let Some(command) = command else {
                    return End::Quit;
                };
                if let Err(error) = on_command(ctx, url, client, state, command).await {
                    tracing::warn!(error = %format_args!("{error:#}"), "server request failed");
                    let text = match error.downcast_ref::<MessageError>() {
                        Some(MessageError::TextLimit) => "Messages can be up to 2,000 characters.",
                        // shortcut: one LiveKit server per process until multi-server voice (W15).
                        _ if error.is::<PinnedElsewhere>() => {
                            "Restart RustTalk to use voice on another server."
                        }
                        _ => FAILED,
                    };
                    ctx.send(Update::Failed(text.into()));
                }
            }
            () = tokio::time::sleep(refresh) => {
                if let Err(error) = client.refresh().await {
                    tracing::warn!(error = %format_args!("{error:#}"), "token refresh failed");
                    return End::Error;
                }
                tracing::info!("session refreshed");
                if let Err(error) = resume(ctx, url, client, state).await {
                    tracing::warn!(error = %format_args!("{error:#}"), "reloading after refresh failed");
                }
            }
            next = next_voice(&mut state.voice) => match next {
                Next::Event(event) => on_voice(ctx, url, client, state, event),
                Next::Capture(capture) => share_screen(ctx, url, client, state, capture).await,
            },
            _ = stats.tick(), if state.voice.is_some() => {
                stop_finished_shares(ctx, url, client, state).await;
                voice_stats(ctx, url, client, state).await;
            }
        }
    }
}

/// Reload what the UI shows after (re)connecting, since events may have been missed.
async fn resume(ctx: &Ctx, url: &Url, client: &mut KeyClient<'_>, state: &mut State) -> Result<()> {
    directory(ctx, url, client, state).await?;
    for channel in state.loaded.clone() {
        page(ctx, url, client, state, channel, None).await;
    }
    set_display_name(ctx, url, client, state).await;
    let voice: Vec<Id> = state
        .last
        .iter()
        .flat_map(|dir| &dir.categories)
        .flat_map(|category| &category.channels)
        .filter(|channel| channel.voice)
        .map(|channel| channel.id)
        .collect();
    for channel in voice {
        if let Err(error) = room_changed(ctx, url, client, state, channel).await {
            tracing::warn!(error = %format_args!("{error:#}"), "loading voice presence failed");
        }
    }
    sync_voice_key(client, state);
    Ok(())
}

async fn on_event(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    event: Event,
) -> Result<()> {
    match event {
        Event::MessageCreated { channel, id } => {
            let (channel, id) = (decode(&channel)?, decode(&id)?);
            if state.loaded.contains(&channel) {
                let record = client.message(&channel, &id).await?;
                let msg = open(client, state, &channel, &record).await;
                send_messages(ctx, url, channel, vec![msg], false, None);
            }
        }
        Event::MessageDeleted { channel, id } => {
            ctx.send(Update::Deleted(
                url.clone(),
                decode(&channel)?,
                decode(&id)?,
            ));
        }
        Event::Resync => resume(ctx, url, client, state).await?,
        Event::MemberChanged { .. } | Event::ChannelChanged | Event::DmCreated { .. } => {
            directory(ctx, url, client, state).await?;
        }
        Event::KeysNeeded(_) => {
            // Names stay unreadable until the space key arrives, which can be after the first load.
            if !state.named && client.has_space_key() {
                directory(ctx, url, client, state).await?;
            }
            for (channel, ids) in std::mem::take(&mut state.waiting) {
                let mut items = Vec::new();
                for id in ids {
                    // The message may have been deleted meanwhile.
                    let Ok(record) = client.message(&channel, &id).await else {
                        continue;
                    };
                    items.push(open(client, state, &channel, &record).await);
                }
                send_messages(ctx, url, channel, items, false, None);
            }
            set_display_name(ctx, url, client, state).await;
        }
        Event::VoiceChanged { channel } => {
            room_changed(ctx, url, client, state, decode(&channel)?).await?;
        }
        Event::RotateNeeded { .. } => {}
    }
    Ok(())
}

async fn on_command(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    command: Command,
) -> Result<()> {
    match command {
        Command::Open(channel) => {
            state.loaded.insert(channel);
            page(ctx, url, client, state, channel, None).await;
        }
        Command::Older(channel, before) => {
            page(ctx, url, client, state, channel, Some(before)).await;
        }
        Command::Send {
            channel,
            text,
            reply_to,
            edit_of,
        } => {
            let body = MessageBody {
                text,
                reply_to,
                edit_of,
            };
            let record = client.send_message(&channel, &body).await?;
            send_messages(ctx, url, channel, vec![to_msg(&record, &body)], false, None);
        }
        Command::Delete(channel, id) => {
            client.delete_message(&channel, &id).await?;
            ctx.send(Update::Deleted(url.clone(), channel, id));
        }
        Command::CreateChannel {
            category,
            new_category,
            voice,
            name,
        } => {
            let category = match category {
                Some(id) => id,
                None => client.create_category(new_category.as_bytes()).await?,
            };
            let kind = if voice { "voice" } else { "text" };
            client
                .create_channel(&category, kind, name.as_bytes(), b"")
                .await?;
            directory(ctx, url, client, state).await?;
        }
        Command::CreateCategory(name) => {
            client.create_category(name.as_bytes()).await?;
            directory(ctx, url, client, state).await?;
        }
        Command::SetServerName(name) => {
            client.set_server_name(name.as_bytes()).await?;
            directory(ctx, url, client, state).await?;
        }
        Command::SetDisplayName(name) => {
            state.display_name = Some(name);
            set_display_name(ctx, url, client, state).await;
        }
        Command::OpenDm(peer) => {
            let existing = state
                .last
                .iter()
                .flat_map(|dir| &dir.dms)
                .find(|dm| dm.peer == peer)
                .map(|dm| dm.channel);
            let channel = match existing {
                Some(channel) => channel,
                None => {
                    let scope = client.create_dm(&peer).await?;
                    directory(ctx, url, client, state).await?;
                    scope.id
                }
            };
            ctx.send(Update::DmReady(url.clone(), channel));
        }
        Command::Invite => {
            let (_, link) = client.invite(INVITE_USES, INVITE_SECONDS).await?;
            ctx.send(Update::Invite(url.clone(), link.to_string()));
        }
        Command::JoinVoice(channel) => {
            leave_voice(ctx, url, client, state);
            let room = match join_voice(ctx, client, &channel).await {
                Ok(room) => room,
                Err(error) => {
                    ctx.send(Update::Call(url.clone(), channel, None));
                    return Err(error);
                }
            };
            tracing::info!(server = %fingerprint(&state.server_id), "joined voice");
            let listening = !room.can_publish();
            let no_screen = !room.can_share_screen();
            state.voice = Some(Live {
                room,
                channel,
                shares: Vec::new(),
                apps: HashMap::new(),
                starting: None,
                screen: None,
                watched: HashSet::new(),
            });
            ctx.send(Update::Call(
                url.clone(),
                channel,
                Some(CallState::Connected),
            ));
            if listening {
                ctx.send(Update::Listening(url.clone(), channel));
            }
            if no_screen {
                ctx.send(Update::NoScreen(url.clone(), channel));
            }
            send_room(ctx, url, client, state, channel);
        }
        Command::LeaveVoice => leave_voice(ctx, url, client, state),
        Command::ShareApp(app) => {
            let Some(live) = &mut state.voice else {
                ctx.send(Update::AppStopped(app.id));
                return Ok(());
            };
            let started = async {
                let target = app.clone();
                // shortcut: starting waits for the app's first audio (up to seconds) and holds
                // this server's loop meanwhile; move it to its own task if that gets noticed.
                let (capture, source) =
                    tokio::task::spawn_blocking(move || share::capture(&target)).await??;
                let sid = live.room.publish_app_audio(&app.name, source).await?.sid();
                anyhow::Ok((capture, sid))
            };
            match started.await {
                Ok((capture, sid)) => {
                    tracing::info!("sharing app audio");
                    live.shares.push(Share { app, sid, capture });
                    let channel = live.channel;
                    send_room(ctx, url, client, state, channel);
                }
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "sharing app audio failed");
                    let text = format!("Couldn't share {}. Is it still playing?", app.name);
                    ctx.send(Update::AppStopped(app.id));
                    ctx.send(Update::Failed(text));
                }
            }
        }
        Command::StopApp(id) => {
            let Some(live) = &mut state.voice else {
                return Ok(());
            };
            let Some(at) = live.shares.iter().position(|share| share.app.id == id) else {
                return Ok(());
            };
            let share = live.shares.remove(at);
            tracing::info!("stopped sharing app audio");
            let channel = live.channel;
            send_room(ctx, url, client, state, channel);
            if let Some(live) = &mut state.voice {
                live.room.unpublish_audio(&share.sid).await?;
            }
        }
        Command::ShareScreen(target, options) => {
            let Some(live) = state.voice.as_mut().filter(|live| live.screen.is_none()) else {
                ctx.send(Update::Sharing(false));
                return Ok(());
            };
            // The system picker waits for the person; keep the server loop running meanwhile.
            live.starting = Some(tokio::spawn(async move {
                // shortcut: always opens a fresh picker; keep the portal's restore token
                // if people ask to skip it when sharing the same screen again.
                let capture = screen::start(target, options, &mut None).await?;
                Ok((capture, options))
            }));
        }
        Command::StopScreen => {
            let Some(live) = &mut state.voice else {
                return Ok(());
            };
            if let Some(starting) = live.starting.take() {
                starting.abort();
            }
            let Some((sid, _capture)) = live.screen.take() else {
                return Ok(());
            };
            tracing::info!("stopped sharing screen");
            let channel = live.channel;
            send_room(ctx, url, client, state, channel);
            if let Some(live) = &mut state.voice {
                live.room.unpublish_screen(&sid).await?;
            }
        }
        Command::Watch(sid, on) => {
            if let Some(live) = &mut state.voice {
                watch(ctx, live, sid, on)?;
            }
        }
    }
    Ok(())
}

/// Start or stop receiving a screen track.
fn watch(ctx: &Ctx, live: &mut Live, sid: String, on: bool) -> Result<()> {
    if on {
        live.watched.insert(sid.clone());
    } else {
        live.watched.remove(&sid);
        ctx.video.remove_track(&sid);
    }
    let Some(video) = live
        .room
        .remote_video_tracks()
        .into_iter()
        .find(|video| video.sid.as_str() == sid)
    else {
        return Ok(());
    };
    live.room.set_subscribed(&video.sid, on)?;
    // Already subscribed: no new event will come.
    if let (true, Some(track)) = (on, video.track) {
        ctx.video.add_track(sid, track.rtc_track());
    }
    Ok(())
}

/// Publish a capture that started, or report why it didn't.
async fn share_screen(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
    state: &mut State,
    capture: Result<(screen::Capture, screen::Options)>,
) {
    let Some(live) = &mut state.voice else {
        return;
    };
    live.starting = None;
    let published = match capture {
        Ok((capture, options)) => live
            .room
            .publish_screen(&capture.name, options, capture.source.clone())
            .await
            .map(|publication| (publication.sid(), capture)),
        Err(error) => Err(error),
    };
    match published {
        Ok(screen) => {
            tracing::info!("sharing screen");
            live.screen = Some(screen);
            ctx.send(Update::Sharing(true));
            let channel = live.channel;
            send_room(ctx, url, client, state, channel);
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "sharing screen failed");
            ctx.send(Update::Sharing(false));
            ctx.send(Update::Failed("Couldn't share your screen.".into()));
        }
    }
}

/// Load one page, newest first on the wire, and send it oldest first.
async fn page(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: Id,
    before: Option<Id>,
) {
    let records = match client.message_page(&channel, before.as_ref(), PAGE).await {
        Ok(records) => records,
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "loading messages failed");
            // Leave the chat loading: reconnecting reloads opened channels, and paging stays paused until then.
            ctx.send(Update::Failed("Couldn't load messages. Try again.".into()));
            return;
        }
    };
    let at_start = records.len() < PAGE as usize;
    let mut items = Vec::with_capacity(records.len());
    for record in records.iter().rev() {
        items.push(open(client, state, &channel, record).await);
    }
    send_messages(ctx, url, channel, items, before.is_some(), Some(at_start));
}

async fn open(
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: &Id,
    record: &MessageRecord,
) -> Msg {
    match client.open_message(channel, record).await {
        Ok(body) => to_msg(record, &body),
        Err(error) => {
            if error.downcast_ref::<MessageError>() == Some(&MessageError::MissingKey) {
                state.waiting.entry(*channel).or_default().insert(record.id);
            }
            tracing::warn!(error = %format_args!("{error:#}"), "message could not be opened");
            Msg {
                id: record.id,
                person: record.person,
                at: record.at,
                reply_to: None,
                edit_of: None,
                body: Body::Unreadable,
            }
        }
    }
}

fn to_msg(record: &MessageRecord, body: &MessageBody) -> Msg {
    Msg {
        id: record.id,
        person: record.person,
        at: record.at,
        reply_to: body.reply_to,
        edit_of: body.edit_of,
        body: Body::Text(body.text.clone()),
    }
}

fn send_messages(
    ctx: &Ctx,
    url: &Url,
    channel: Id,
    items: Vec<Msg>,
    older: bool,
    at_start: Option<bool>,
) {
    ctx.send(Update::Messages {
        server: url.clone(),
        channel,
        items,
        older,
        at_start,
    });
}

/// Set a display name chosen before the space key arrived, once it has.
async fn set_display_name(ctx: &Ctx, url: &Url, client: &mut KeyClient<'_>, state: &mut State) {
    let Some(name) = &state.display_name else {
        return;
    };
    if !client.has_space_key() {
        return;
    }
    match client.set_display_name(name.as_bytes()).await {
        Ok(()) => {
            state.display_name = None;
            if let Err(error) = directory(ctx, url, client, state).await {
                tracing::warn!(error = %format_args!("{error:#}"), "reloading members failed");
            }
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "setting display name failed");
            state.display_name = None;
            ctx.send(Update::Failed(
                "Couldn't change your name. Try again.".into(),
            ));
        }
    }
}

async fn get(client: &KeyClient<'_>, path: &str) -> Result<Value> {
    Ok(client
        .request(Method::GET, path)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Fetch and decrypt names, channels, members and permissions, and report member changes.
async fn directory(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, state: &mut State) -> Result<()> {
    let catalog = client.catalog().await?;
    let members = get(client, "/v1/members").await?;
    let me = get(client, "/v1/me").await?;
    let roles = get(client, "/v1/roles").await?;
    let space = get(client, "/v1/space").await?;
    let dir = build(client, &catalog, &members, &me, &roles, &space)?;
    if let Some(before) = &state.last {
        for (person, action, detail) in changes(before, &dir) {
            let name = member_name(&dir, &person);
            let name = if dir.members.iter().any(|m| m.person == person) {
                name
            } else {
                member_name(before, &person)
            };
            state.notices += 1;
            let mut id = [0xff; 32];
            id[..8].copy_from_slice(&state.notices.to_be_bytes());
            let msg = Msg {
                id,
                person,
                at: now(),
                reply_to: None,
                edit_of: None,
                body: Body::Notice {
                    name,
                    action,
                    detail,
                },
            };
            ctx.send(Update::Notice(url.clone(), person, msg));
        }
    }
    let joined = state.voice.as_ref().map(|live| live.channel);
    let deleted = joined.filter(|joined| {
        !dir.categories
            .iter()
            .flat_map(|category| &category.channels)
            .any(|channel| channel.id == *joined)
    });
    state.last = Some(dir.clone());
    state.named = client.has_space_key();
    ctx.send(Update::Directory(url.clone(), Box::new(dir)));
    if let Some(channel) = deleted {
        leave_voice(ctx, url, client, state);
        ctx.send(Update::Call(url.clone(), channel, None));
    }
    Ok(())
}

fn build(
    client: &KeyClient<'_>,
    catalog: &Catalog,
    members: &Value,
    me: &Value,
    roles: &Value,
    space: &Value,
) -> Result<Directory> {
    let text = |kind: Metadata, id: &[u8], ct: Option<&str>| {
        let plain = client.decrypt_metadata(kind, id, ct?);
        if let Err(error) = &plain {
            tracing::warn!(error = %format_args!("{error:#}"), "name could not be decrypted");
        }
        Some(String::from_utf8_lossy(&plain.ok()?).into_owned())
    };
    let strings = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    };
    let my_roles = strings(&me["role_ids"]);
    let mut perms = Perms {
        owner: my_roles.iter().any(|role| role == "owner"),
        ..Perms::default()
    };
    for role in roles["roles"].as_array().into_iter().flatten() {
        if role["id"]
            .as_str()
            .is_some_and(|id| my_roles.iter().any(|mine| mine == id))
        {
            for permission in strings(&role["permissions"]) {
                match permission.as_str() {
                    "manage_channels" => perms.manage_channels = true,
                    "invite" => perms.invite = true,
                    "delete_messages" => perms.delete_messages = true,
                    _ => {}
                }
            }
        }
    }

    let mut categories: Vec<_> = catalog.categories.iter().collect();
    categories.sort_by_key(|category| (category.position, category.id));
    let mut categories: Vec<Category> = categories
        .into_iter()
        .map(|category| Category {
            id: Some(category.id),
            name: text(
                Metadata::CategoryName,
                &category.id,
                Some(&category.name_ct),
            )
            .unwrap_or_else(|| "Channels".into()),
            channels: Vec::new(),
        })
        .collect();
    let mut loose = Vec::new();
    let mut dms = Vec::new();
    let mut channels: Vec<_> = catalog.channels.iter().collect();
    channels.sort_by_key(|channel| (channel.position, channel.id));
    for channel in channels {
        if channel.kind == "dm" {
            if let Some(peer) = channel.peer.as_deref() {
                dms.push(Dm {
                    channel: channel.id,
                    peer: decode(peer)?,
                });
            }
            continue;
        }
        let entry = Channel {
            id: channel.id,
            name: text(
                Metadata::ChannelName,
                &channel.id,
                channel.name_ct.as_deref(),
            )
            .unwrap_or_else(|| "channel".into()),
            voice: channel.kind == "voice",
        };
        let category = categories.iter_mut().find(|category| {
            category
                .id
                .is_some_and(|id| channel.category_id.as_deref() == Some(encode(&id).as_str()))
        });
        match category {
            Some(category) => category.channels.push(entry),
            None => loose.push(entry),
        }
    }
    // Text channels first, then voice, like other chat apps.
    loose.sort_by_key(|channel| channel.voice);
    for category in &mut categories {
        category.channels.sort_by_key(|channel| channel.voice);
    }
    // Stable, so voice-only categories follow the rest in server order.
    categories.sort_by_key(|c| !c.channels.is_empty() && c.channels.iter().all(|ch| ch.voice));
    if !loose.is_empty() {
        categories.insert(
            0,
            Category {
                id: None,
                name: String::new(),
                channels: loose,
            },
        );
    }

    let mut list = Vec::new();
    for row in members.as_array().into_iter().flatten() {
        let person: Id = decode(row["person_id"].as_str().context("member without ID")?)?;
        let devices = client
            .devices()
            .filter(|device| device.person_id() == &person)
            .map(|device| Device {
                name: device.name().to_owned(),
                fingerprint: fingerprint(device.signing_key().as_bytes()),
            })
            .collect();
        list.push(Member {
            person,
            name: text(
                Metadata::DisplayName,
                &person,
                row["display_name_ct"].as_str(),
            ),
            devices,
        });
    }
    list.sort_by_cached_key(|member| {
        (
            member.name.is_none(),
            member.name.as_ref().map(|n| n.to_lowercase()),
        )
    });
    let server_id = client.space().id;
    Ok(Directory {
        name: text(Metadata::ServerName, &server_id, space["name_ct"].as_str()),
        me: *client.identity.certificate.person_id(),
        perms,
        categories,
        members: list,
        dms,
    })
}

/// Member and device changes between two directory loads: (person, action, details).
fn changes(before: &Directory, after: &Directory) -> Vec<(Id, &'static str, String)> {
    let mut out = Vec::new();
    for member in &after.members {
        let id = format!("ID {}", fingerprint(&member.person));
        let Some(old) = before
            .members
            .iter()
            .find(|old| old.person == member.person)
        else {
            out.push((member.person, "joined", id));
            continue;
        };
        // A new member's first device arrives just after the join; that is not news.
        if old.devices.is_empty() {
            continue;
        }
        for device in &member.devices {
            if old
                .devices
                .iter()
                .all(|d| d.fingerprint != device.fingerprint)
            {
                let detail = format!("{id}\nNew device {} ({})", device.fingerprint, device.name);
                out.push((member.person, "added a new device", detail));
            }
        }
        for device in &old.devices {
            if member
                .devices
                .iter()
                .all(|d| d.fingerprint != device.fingerprint)
            {
                let detail = format!(
                    "{id}\nRemoved device {} ({})",
                    device.fingerprint, device.name
                );
                out.push((member.person, "removed a device", detail));
            }
        }
    }
    for old in &before.members {
        if after
            .members
            .iter()
            .all(|member| member.person != old.person)
        {
            let id = format!("ID {}", fingerprint(&old.person));
            out.push((old.person, "left", id));
        }
    }
    out
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// Serializes edits of joined.json across server tasks.
static JOINED: Mutex<()> = Mutex::new(());

fn load_joined(data: &Path) -> Result<Vec<KnownServer>> {
    match fs::read(data.join("joined.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("read joined servers"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("open joined servers"),
    }
}

fn edit_joined(data: &Path, edit: impl FnOnce(&mut Vec<KnownServer>)) -> Result<()> {
    let _guard = JOINED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut list = load_joined(data)?;
    edit(&mut list);
    save_json(data, &data.join("joined.json"), &list).context("save joined servers")
}

/// Connect to a voice channel and publish the microphone when allowed to talk.
async fn join_voice(ctx: &Ctx, client: &mut KeyClient<'_>, channel: &Id) -> Result<VoiceRoom> {
    let room = client.join_voice(channel).await?;
    if room.can_publish() {
        if let Err(error) = room.publish_audio(ctx.audio.source()).await {
            leave_room(room).await;
            return Err(error);
        }
    } else {
        ctx.send(Update::Failed("You can listen here, but not talk.".into()));
    }
    Ok(room)
}

fn leave_voice(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, state: &mut State) {
    let Some(live) = state.voice.take() else {
        return;
    };
    for sid in &live.watched {
        ctx.video.remove_track(sid);
    }
    // Closes a system picker that is still open.
    if let Some(starting) = &live.starting {
        starting.abort();
    }
    // Closing waits for the media server; nothing here needs to wait for it.
    tokio::spawn(leave_room(live.room));
    send_room(ctx, url, client, state, live.channel);
}

async fn leave_room(room: VoiceRoom) {
    tracing::info!("left voice");
    if let Err(error) = room.leave().await {
        tracing::warn!(error = %format_args!("{error:#}"), "leaving voice failed");
    }
}

/// Install a newer media key after key work for the voice channel.
fn sync_voice_key(client: &KeyClient<'_>, state: &mut State) {
    if let Some(live) = &mut state.voice
        && let Err(error) = live.room.sync_key(client)
    {
        tracing::warn!(error = %format_args!("{error:#}"), "voice key rotation failed");
    }
}

// Lives only between a select and its handler.
#[allow(clippy::large_enum_variant)]
enum Next {
    Event(Option<RoomEvent>),
    /// A screen capture finished starting; clear `starting` before polling again.
    Capture(Result<(screen::Capture, screen::Options)>),
}

async fn next_voice(voice: &mut Option<Live>) -> Next {
    let Some(live) = voice else {
        return std::future::pending().await;
    };
    let starting = async {
        match live.starting.as_mut() {
            Some(starting) => starting.await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        event = live.room.next_event() => Next::Event(event),
        capture = starting => Next::Capture(capture.map_err(anyhow::Error::from).and_then(|c| c)),
    }
}

/// LiveKit identity of every known device, to find the person behind a voice participant.
fn identities(client: &KeyClient<'_>, server: &Id) -> HashMap<String, Id> {
    client
        .devices()
        .map(|device| {
            let id = voice_identity(server, device.signing_key().as_bytes());
            (id, *device.person_id())
        })
        .collect()
}

async fn room_changed(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: Id,
) -> Result<()> {
    let people = client.voice_participants(&channel).await?;
    state.rooms.insert(channel, people);
    send_room(ctx, url, client, state, channel);
    Ok(())
}

/// The server's roster for a voice channel, plus who the room itself shows,
/// with "connecting" for anyone whose audio isn't flowing in our room.
fn send_room(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, state: &State, channel: Id) {
    let me = *ctx.identity.certificate.person_id();
    let roster = state.rooms.get(&channel).cloned().unwrap_or_default();
    let mut people = roster.people;
    let mut peers = Vec::new();
    match state.voice.as_ref().filter(|live| live.channel == channel) {
        None => peers.extend(people.into_iter().map(|person| Peer {
            person,
            connecting: false,
            unknown: false,
            apps: Vec::new(),
            screens: Vec::new(),
        })),
        Some(live) => {
            let ids = identities(client, &state.server_id);
            let mut live_people: HashMap<Id, bool> = HashMap::new();
            let mut apps: HashMap<Id, Vec<(TrackSid, String)>> = HashMap::new();
            let mut screens: HashMap<Id, Vec<Screen>> = HashMap::new();
            for (participant, voice) in live.room.participants() {
                if let Some(person) = ids.get(participant.identity().as_str()) {
                    *live_people.entry(*person).or_default() |= voice == VoiceState::Connected;
                    apps.entry(*person).or_default().extend(
                        participant
                            .track_publications()
                            .into_keys()
                            .filter_map(|sid| {
                                let name = live.apps.get(&sid)?.clone();
                                Some((sid, name))
                            }),
                    );
                }
            }
            for names in apps.values_mut() {
                names.sort_by(|a, b| a.1.cmp(&b.1));
            }
            apps.insert(
                me,
                live.shares
                    .iter()
                    .map(|share| (share.sid.clone(), share.app.name.clone()))
                    .collect(),
            );
            for video in live.room.remote_video_tracks() {
                if video.source == TrackSource::Screenshare
                    && let Some(person) = ids.get(video.participant.as_str())
                {
                    screens.entry(*person).or_default().push(Screen {
                        sid: video.sid.to_string(),
                        name: video.name,
                    });
                }
            }
            if let Some((sid, _)) = &live.screen {
                screens.insert(
                    me,
                    vec![Screen {
                        sid: sid.to_string(),
                        name: "Your screen".into(),
                    }],
                );
            }
            people.extend(live_people.keys().copied());
            people.push(me);
            people.sort();
            people.dedup();
            let connected = live.room.connection_state() == ConnectionState::Connected;
            for person in people {
                let ok = if person == me {
                    connected
                } else {
                    live_people.get(&person) == Some(&true)
                };
                peers.push(Peer {
                    person,
                    connecting: !ok,
                    unknown: false,
                    apps: apps.remove(&person).unwrap_or_default(),
                    screens: screens.remove(&person).unwrap_or_default(),
                });
            }
        }
    }
    peers.extend((0..roster.unknown).map(|_| Peer {
        person: [0; 32],
        connecting: false,
        unknown: true,
        apps: Vec::new(),
        screens: Vec::new(),
    }));
    ctx.send(Update::Room(url.clone(), channel, peers));
}

fn on_voice(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
    state: &mut State,
    event: Option<RoomEvent>,
) {
    let Some(channel) = state.voice.as_ref().map(|live| live.channel) else {
        return;
    };
    let end = |state: &mut State, text: &str| {
        if let Some(live) = state.voice.take() {
            for sid in &live.watched {
                ctx.video.remove_track(sid);
            }
        }
        ctx.send(Update::Call(url.clone(), channel, None));
        ctx.send(Update::Failed(text.into()));
    };
    let Some(event) = event else {
        tracing::warn!("voice room closed");
        end(state, "Voice disconnected.");
        send_room(ctx, url, client, state, channel);
        return;
    };
    match event {
        RoomEvent::TrackSubscribed {
            track: RemoteTrack::Audio(track),
            participant,
            ..
        } => {
            let ids = identities(client, &state.server_id);
            let person = ids.get(participant.identity().as_str()).copied();
            if person.is_none() {
                tracing::warn!("voice track from a device this client doesn't know yet");
            }
            let sid = track.sid();
            let mut app = None;
            if let Some(live) = &mut state.voice
                && let Some(name) = live.room.app_audio_name(&track)
            {
                live.apps.insert(sid.clone(), name);
                app = Some(sid.clone());
            }
            ctx.audio.add_track(
                sid.to_string(),
                (person.unwrap_or_default(), app),
                track.rtc_track(),
            );
        }
        RoomEvent::TrackSubscribed {
            track: RemoteTrack::Video(track),
            publication,
            ..
        } => {
            let sid = track.sid().to_string();
            match state.voice.as_ref() {
                Some(live) if live.watched.contains(&sid) => {
                    ctx.video.add_track(sid, track.rtc_track());
                }
                // Watching stopped while subscribing.
                Some(live) => {
                    if let Err(error) = live.room.set_subscribed(&publication.sid(), false) {
                        tracing::warn!(error = %format_args!("{error:#}"), "unsubscribing a screen failed");
                    }
                }
                None => {}
            }
        }
        RoomEvent::TrackUnsubscribed { track, .. } => {
            if let Some(live) = &mut state.voice {
                live.apps.remove(&track.sid());
            }
            ctx.audio.remove_track(track.sid().as_str());
            ctx.video.remove_track(track.sid().as_str());
        }
        RoomEvent::Reconnecting => {
            tracing::info!("voice reconnecting");
            ctx.send(Update::Call(
                url.clone(),
                channel,
                Some(CallState::Reconnecting),
            ));
        }
        RoomEvent::Reconnected => {
            tracing::info!("voice reconnected");
            ctx.send(Update::Call(
                url.clone(),
                channel,
                Some(CallState::Connected),
            ));
        }
        RoomEvent::Disconnected { reason } => {
            tracing::info!(?reason, "voice disconnected");
            end(state, "Voice disconnected.");
        }
        RoomEvent::ParticipantConnected(_)
        | RoomEvent::ParticipantActive(_)
        | RoomEvent::TrackPublished { .. }
        | RoomEvent::TrackUnpublished { .. }
        | RoomEvent::ParticipantDisconnected(_)
        | RoomEvent::ConnectionStateChanged(_) => {}
        _ => return,
    }
    send_room(ctx, url, client, state, channel);
}

/// Stop sharing apps that closed or whose capture stopped by itself.
async fn stop_finished_shares(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, state: &mut State) {
    let Some(live) = &mut state.voice else {
        return;
    };
    let (ended, kept): (Vec<Share>, _) = std::mem::take(&mut live.shares)
        .into_iter()
        .partition(|share| share.capture.is_finished());
    live.shares = kept;
    if live
        .screen
        .as_ref()
        .is_some_and(|(_, capture)| capture.is_finished())
    {
        let (sid, _) = live.screen.take().expect("checked above");
        tracing::info!("screen capture ended");
        if let Err(error) = live.room.unpublish_screen(&sid).await {
            tracing::warn!(error = %format_args!("{error:#}"), "stopping screen share failed");
        }
        ctx.send(Update::Sharing(false));
    } else if ended.is_empty() {
        return;
    }
    for share in &ended {
        tracing::info!("app audio capture ended");
        if let Err(error) = live.room.unpublish_audio(&share.sid).await {
            tracing::warn!(error = %format_args!("{error:#}"), "stopping app audio failed");
        }
        ctx.send(Update::AppStopped(share.app.id.clone()));
    }
    let channel = live.channel;
    send_room(ctx, url, client, state, channel);
}

/// The connection panel's numbers, from the SDK's WebRTC stats.
async fn voice_stats(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, state: &mut State) {
    let Some(live) = &mut state.voice else {
        return;
    };
    let snapshot = match live.room.stats().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "reading voice stats failed");
            return;
        }
    };
    let ids = identities(client, &state.server_id);
    let me = *ctx.identity.certificate.person_id();
    let screens: HashMap<TrackSid, String> = live
        .screen
        .iter()
        .map(|(sid, _)| (sid.clone(), "Screen".into()))
        .chain(
            live.room
                .remote_video_tracks()
                .into_iter()
                .map(|video| (video.sid, video.name)),
        )
        .collect();
    let (sent, received): (Vec<_>, Vec<_>) = snapshot.tracks.iter().partition(|t| t.outbound);
    let lost = |tracks: &[&rusttalk_core::client::voice::TrackStats]| {
        tracks
            .iter()
            .filter_map(|t| t.packets_lost)
            .reduce(|a, b| a + b)
    };
    let stats = Stats {
        ping_ms: snapshot.ping_ms,
        jitter_ms: received.iter().filter_map(|t| t.jitter_ms).reduce(f64::max),
        lost_in: lost(&received),
        lost_out: lost(&sent),
        tracks: snapshot
            .tracks
            .iter()
            .filter_map(|t| {
                Some(TrackStats {
                    person: if t.outbound {
                        me
                    } else {
                        *ids.get(t.participant.as_str())?
                    },
                    mine: t.outbound,
                    app: live
                        .apps
                        .get(&t.sid)
                        .or_else(|| screens.get(&t.sid))
                        .or_else(|| {
                            let share = live.shares.iter().find(|share| share.sid == t.sid)?;
                            Some(&share.app.name)
                        })
                        .cloned(),
                    kbps: t.bitrate_bps? / 1000.0,
                })
            })
            .collect(),
    };
    ctx.send(Update::Stats(url.clone(), stats));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(person: u8, devices: &[&str]) -> Member {
        Member {
            person: [person; 32],
            name: Some(format!("P{person}")),
            devices: devices
                .iter()
                .map(|fp| Device {
                    name: "Desktop".into(),
                    fingerprint: (*fp).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn reports_joins_leaves_and_device_changes_but_not_a_first_device() {
        let dir = |members| Directory {
            members,
            ..Directory::default()
        };
        let before = dir(vec![member(1, &["a"]), member(2, &[]), member(3, &["c"])]);
        let after = dir(vec![
            member(1, &["a", "b"]),
            member(2, &["d"]),
            member(4, &[]),
        ]);
        let lines: Vec<_> = changes(&before, &after)
            .into_iter()
            .map(|(person, action, _)| (person[0], action))
            .collect();
        assert_eq!(
            lines,
            [(1, "added a new device"), (4, "joined"), (3, "left")]
        );
    }
}
