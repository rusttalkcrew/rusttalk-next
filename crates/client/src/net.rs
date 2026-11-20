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
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::{InviteCode, InviteLink, KnownServer, ProtocolError, Session, decode, encode},
    client::{
        Catalog, Event, KeyClient, MessageBody, MessageError, MessageRecord, Server as Api,
        invite_name, save_json,
    },
    identity::{DeviceIdentity, fingerprint},
    keys::Metadata,
};
use serde_json::Value;
use tokio::{
    runtime::Runtime,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};
use url::Url;

use crate::server::{
    Body, CallState, Category, Channel, Device, Directory, Dm, Id, Member, Msg, Peer, Perms, Stats,
    Status, member_name,
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
    /// Everyone in one voice channel.
    #[allow(dead_code)]
    Room(Url, Id, Vec<Peer>),
    #[allow(dead_code)]
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
}

impl Ctx {
    fn send(&self, update: Update) {
        if self.updates.send(update).is_ok() {
            self.egui.request_repaint();
        }
    }
}

impl Net {
    pub fn new(egui: egui::Context, identity: Arc<DeviceIdentity>, data: PathBuf) -> Result<Self> {
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
            },
            updates: receiver,
        })
    }

    /// For audio work that must outlive a frame, like reading remote tracks.
    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.runtime
            .as_ref()
            .expect("set until drop")
            .handle()
            .clone()
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
                    Some(_) => ctx.send(Update::Failed(OFFLINE.into())),
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
    loop {
        // Refresh ten minutes before the 12-hour token expires.
        let refresh = Duration::from_secs((client.expires_at() - 600 - now()).max(0) as u64);
        tokio::select! {
            event = client.next_event() => match event {
                Ok(Some(event)) => {
                    if let Err(error) = on_event(ctx, url, client, state, event).await {
                        tracing::warn!(error = %format_args!("{error:#}"), "handling a server event failed");
                    }
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
        // Voice presence is shown by the voice UI (W08 client), not the chat directory.
        Event::RotateNeeded { .. } | Event::VoiceChanged { .. } => {}
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
        // Stand-in until the core voice module is wired.
        Command::JoinVoice(channel) => {
            ctx.send(Update::Call(url.clone(), channel, None));
            ctx.send(Update::Failed("Voice isn't ready yet.".into()));
        }
        Command::LeaveVoice => {}
    }
    Ok(())
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
    state.last = Some(dir.clone());
    state.named = client.has_space_key();
    ctx.send(Update::Directory(url.clone(), Box::new(dir)));
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
