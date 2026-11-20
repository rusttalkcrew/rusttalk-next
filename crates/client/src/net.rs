//! Network work for joined servers. It runs on a tokio runtime off the UI thread:
//! the UI sends each server task a `Command` and drains `Update`s every frame.
//! Never log names, message text or invite links here.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use eframe::egui;
use livekit::prelude::Participant;
use reqwest::StatusCode;
use rusttalk_core::{
    auth::{Fingerprint, InviteCode, InviteLink, KnownServer, ProtocolError, Session, decode},
    camera::CameraOptions,
    client::{
        BlobRef, Event, KeyClient, MessageBody, MessageError, MessageRecord, Server as Api,
        invite_name,
        voice::{LiveFileEvent, NativeVideoSource, RoomEvent, VoiceRoster},
    },
    identity::{DeviceIdentity, fingerprint},
};
use serde_json::Value;
use tokio::{
    runtime::Runtime,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};
use url::Url;

use crate::{
    audio::Audio,
    device_sync::{DeviceSync, Edit, Synced},
    screen::{self, Video},
    server::{Body, CallState, Directory, Id, Msg, Peer, Stats, Status},
    share::App,
};

mod admin;
mod directory;
mod files;
mod find;
mod voice;

pub use admin::{Admin, AdminUpdate, AuditEntry, Ban, PublicListing};
use directory::{directory, get};
pub use files::{MAX_FILE, file_name};
pub use find::{Listed, PAGE_SIZE};
use voice::{
    Live, Next, leave_voice, next_voice, on_voice, room_changed, send_room, share_screen,
    stop_finished_shares, stop_lost_video, sync_voice_key, voice_command, voice_only, voice_stats,
};

const PAGE: u32 = 50;
const CHANGED: &str = "This server changed. Don't continue unless the owner told you it would.";
const OFFLINE: &str = "You're offline. Try again in a moment.";
const FAILED: &str = "That didn't work. Try again.";

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
        /// Uploaded first, then sent with the message.
        files: Vec<PathBuf>,
    },
    /// Channel, message, and the IDs of its stored files to remove too.
    Delete(Id, Id, Id, Vec<Id>),
    /// Download and decode a picture: a thumbnail, or the full image (`true`).
    Image(BlobRef, bool),
    /// Download a file and save it here.
    Save(BlobRef, PathBuf),
    /// Offer a file to everyone in the voice room, sent through the call.
    OfferFile(PathBuf),
    /// Receive an offered file into this path.
    AcceptFile(Id, PathBuf),
    /// Stop our offer, or our download, of a live file.
    CancelFile(Id),
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
    /// Make an invite link: uses and lifetime in seconds.
    Invite(u32, u32),
    Admin(Admin),
    JoinVoice(Id),
    LeaveVoice,
    ShareApp(App),
    /// Stop sharing an app, by its ID.
    StopApp(String),
    /// Share a screen or window, or open the system picker (Linux).
    ShareScreen(screen::Target, screen::Options),
    StopScreen,
    /// Start or stop receiving a screen or camera, by track.
    Watch(String, bool),
    /// Publish a running camera, replacing an earlier one.
    ShareCamera(NativeVideoSource, CameraOptions),
    StopCamera,
}

pub struct LiveOffer {
    pub id: Id,
    /// Who offers it; `None` for our own offer.
    pub from: Option<Id>,
    pub name: String,
    pub size: u64,
}

pub struct Preview {
    pub server: KnownServer,
    /// The invite to join with; `None` when it expired but the server lets anyone join.
    pub invite: Option<InviteCode>,
    /// The server name from the invite, when the invite carries one.
    pub name: Option<String>,
    /// This profile has connected to this address before.
    pub known: bool,
    /// The server lets people join without an invite.
    pub open: bool,
}

/// How a new member gets in the first time.
pub enum Admission {
    Invite(InviteCode),
    /// The server lets anyone join.
    Open,
}

pub enum Update {
    Preview(Result<Box<Preview>, String>),
    /// A page of directory results, by page number.
    Found(u32, Result<Vec<Listed>, String>),
    /// A server picked from the directory, checked; errors show in the directory.
    Listed(Result<Box<Preview>, String>),
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
    /// A new invite: its ID (to revoke it) and link.
    Invite(Url, String, String),
    Admin(Url, AdminUpdate),
    /// The call in a voice channel changed state; `None` when it ended.
    Call(Url, Id, Option<CallState>),
    /// This device joined the call without permission to talk.
    Listening(Url, Id),
    /// This device may not share its screen or camera in the call.
    NoVideo(Url, Id),
    /// Everyone in one voice channel.
    Room(Url, Id, Vec<Peer>),
    /// Sharing this app stopped or never started.
    AppStopped(String),
    /// This device's screen share went live (`true`), or stopped or never started.
    Sharing(bool),
    /// This device's camera stopped publishing or never started.
    CameraStopped,
    Stats(Url, Stats),
    /// A file upload in a channel started (with what to show) or ended.
    Uploading(Url, Id, Option<String>),
    /// A picture by blob ID, or why not: `true` when it expired or was deleted.
    Image(Id, Result<egui::ColorImage, bool>),
    /// A file by blob ID was saved, or why not: `true` when it expired or was deleted.
    Saved(Id, Result<(), bool>),
    /// A file offered in the voice room, by someone else or by us.
    LiveOffer(LiveOffer),
    /// Someone started (`true`) or stopped receiving our live file.
    LiveReceiver(Id, Id, bool),
    /// How receiving a live file goes.
    LiveProgress(Id, LiveFileEvent),
    /// This device may not send files in the call.
    NoFiles(Url, Id),
    /// The joined servers and synced settings after a device sync.
    Synced(Box<Synced>),
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
    sync: Arc<DeviceSync>,
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
        let sync = Arc::new(DeviceSync::open(&data)?);
        Ok(Self {
            runtime: Some(runtime),
            ctx: Ctx {
                updates,
                egui,
                identity,
                data: data.into(),
                audio,
                video,
                sync,
            },
            updates: receiver,
        })
    }

    /// Servers this profile joined earlier, to reconnect at start.
    pub fn joined(&self) -> Vec<KnownServer> {
        self.ctx.sync.servers()
    }

    /// Check an invite's server and read its name, without joining yet.
    pub fn preview(&self, link: InviteLink) {
        let ctx = self.ctx.clone();
        self.spawn(async move {
            let update = preview(&ctx, &link).await.map(Box::new);
            ctx.send(Update::Preview(update));
        });
    }

    /// Search the server directory. Only called while the person has "Find servers" open.
    pub fn search(&self, index: Url, pin: Option<Fingerprint>, query: String, page: u32) {
        let ctx = self.ctx.clone();
        self.spawn(async move {
            let found = find::search(&index, pin, &query, page).await;
            ctx.send(Update::Found(page, found));
        });
    }

    /// Check a server picked from the directory, without joining yet.
    pub fn check_listed(&self, listed: Listed) {
        let ctx = self.ctx.clone();
        self.spawn(async move {
            let update = find::preview(&ctx, listed).await.map(Box::new);
            ctx.send(Update::Listed(update));
        });
    }

    /// Connect to a server (joining with `admission` the first time) and keep it connected.
    pub fn start(
        &self,
        server: KnownServer,
        admission: Option<Admission>,
        display_name: Option<String>,
    ) -> UnboundedSender<Command> {
        let (commands, receiver) = unbounded_channel();
        let ctx = self.ctx.clone();
        self.spawn(run(ctx, server, admission, display_name, receiver));
        commands
    }

    /// Publish local edits to this person's other devices.
    pub fn sync(&self, edits: Vec<Edit>) {
        let ctx = self.ctx.clone();
        self.spawn(async move { sync(&ctx, edits).await });
    }

    /// Leave a server on all devices. Its connection stays until the leave is published there.
    pub fn leave(&self, address: Url, commands: UnboundedSender<Command>) {
        let ctx = self.ctx.clone();
        self.spawn(async move {
            sync(&ctx, vec![Edit::Leave(address)]).await;
            drop(commands);
        });
    }

    /// The network runtime, for LiveKit objects created off it (camera sources).
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime
            .as_ref()
            .expect("runtime until drop")
            .handle()
            .clone()
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

async fn preview(ctx: &Ctx, link: &InviteLink) -> Result<Preview, String> {
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
    let open = open_joining(&api).await;
    let mut invite = Some(link.code);
    let name = match &link.link_key {
        // Owner invites from the server console carry no name.
        None => None,
        Some(_) => match invite_name(&api, link).await {
            Ok(name) => name.map(|name| String::from_utf8_lossy(&name).into_owned()),
            // A used-up invite still names the server, which may let anyone join.
            Err(error) if open => {
                tracing::info!(error = %format_args!("{error:#}"), "invite spent, server is open");
                invite = None;
                None
            }
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "invite preview failed");
                return Err(
                    "This invite has expired or was already used. Ask for a new one.".into(),
                );
            }
        },
    };
    tracing::info!(server = %fingerprint(&link.server.server_id), known = known.is_some(), open, "invite checked");
    Ok(Preview {
        server: link.server.clone(),
        invite,
        name,
        known: known.is_some(),
        open,
    })
}

/// Whether the server lets anyone join; `false` when it can't tell.
async fn open_joining(api: &Api) -> bool {
    match api.info().await {
        Ok(info) => info.open_joining,
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "reading open joining failed");
            false
        }
    }
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
    mut admission: Option<Admission>,
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
        match session(&ctx, &server, &mut api, admission.as_ref()).await {
            Ok((api, session)) => {
                if admission.take().is_some() {
                    tracing::info!(server = %fingerprint(&server.server_id), "joined server");
                    let joined = vec![Edit::Join(api.known_server().clone())];
                    if let Err(error) = ctx.sync.edit(&ctx.data, joined).await {
                        tracing::error!(error = %format_args!("{error:#}"), "saving joined server failed");
                    }
                }
                match KeyClient::connect(api, &ctx.identity, &session).await {
                    Ok(mut client) => {
                        delay = 1;
                        tracing::info!(server = %fingerprint(&server.server_id), "connected to server");
                        ctx.send(Update::Status(url.clone(), Status::Online));
                        let end = serve(&ctx, &url, &mut client, &mut state, &mut commands).await;
                        ctx.sync.set_peer(&url, None);
                        match end {
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
                    // The voice room outlives the API connection, so its shares can still stop.
                    Some(command) if voice_only(&command) => {
                        if let Err(error) = voice_command(&ctx, &mut state, command).await {
                            tracing::warn!(error = %format_args!("{error:#}"), "voice request failed");
                        }
                    }
                    Some(Command::ShareApp(app)) => {
                        ctx.send(Update::AppStopped(app.id));
                        ctx.send(Update::Failed(OFFLINE.into()));
                    }
                    Some(Command::ShareScreen(..)) => {
                        ctx.send(Update::Sharing(false));
                        ctx.send(Update::Failed(OFFLINE.into()));
                    }
                    Some(Command::ShareCamera(..)) => {
                        ctx.send(Update::CameraStopped);
                        ctx.send(Update::Failed(OFFLINE.into()));
                    }
                    Some(Command::Admin(_)) => {
                        ctx.send(Update::Admin(url.clone(), AdminUpdate::Failed(OFFLINE.into())));
                    }
                    Some(Command::Image(file, _)) => ctx.send(Update::Image(file.id, Err(false))),
                    Some(Command::Save(file, _)) => ctx.send(Update::Saved(file.id, Err(false))),
                    Some(Command::AcceptFile(id, _)) => {
                        let reason = OFFLINE.into();
                        ctx.send(Update::LiveProgress(id, LiveFileEvent::Failed { reason }));
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
    admission: Option<&Admission>,
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
    let reply = match admission {
        Some(Admission::Open) => api.join_open(&ctx.identity).await,
        Some(Admission::Invite(code)) => api.sign_in(&ctx.identity, Some(code)).await,
        None => api.sign_in(&ctx.identity, None).await,
    }
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
        "invite_required" if admission.is_none() => {
            Failure::Stop("You're no longer a member of this server.".into())
        }
        "invite_required" | "invalid_invite" | "invite_not_found" => {
            Failure::Stop("This invite has expired or was already used. Ask for a new one.".into())
        }
        "person_banned" | "ip_banned" => Failure::Stop("You're banned from this server.".into()),
        "open_joining_disabled" => {
            Failure::Stop("This server needs an invite now. Ask someone for one.".into())
        }
        "member_cap_reached" => {
            Failure::Stop("This server is full. Ask someone there for an invite.".into())
        }
        "open_join_limit" => Failure::Stop(
            "Too many people joined this server recently. Try again in an hour.".into(),
        ),
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
                Next::Event(event) => {
                    let lost = match &event {
                        Some(RoomEvent::ParticipantPermissionChanged {
                            participant: Participant::Local(_),
                            ..
                        }) => Some(None),
                        Some(RoomEvent::LocalTrackUnpublished { publication, .. }) => {
                            Some(Some(publication.sid()))
                        }
                        _ => None,
                    };
                    on_voice(ctx, url, client, state, event);
                    if let Some(unpublished) = lost {
                        stop_lost_video(ctx, url, client, state, unpublished).await;
                    }
                }
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
    // Also after each token refresh, which picks up changes from other devices twice a day.
    let (api, session) = client.sync_peer();
    ctx.sync.set_peer(url, Some((api.clone(), session.clone())));
    let ctx = ctx.clone();
    tokio::spawn(async move { sync(&ctx, Vec::new()).await });
    Ok(())
}

/// Merge with this person's other devices and show the result.
async fn sync(ctx: &Ctx, edits: Vec<Edit>) {
    let result = ctx
        .sync
        .run(&ctx.data, &ctx.identity.sync_key, edits, |synced| {
            for address in &synced.conflicts {
                let server = address.host_str().unwrap_or("a server");
                ctx.send(Update::Failed(format!(
                    "Your other device trusts a different key for {server}. Check with the server owner."
                )));
            }
            ctx.send(Update::Synced(Box::new(synced)));
        })
        .await;
    if let Err(error) = result {
        tracing::error!(error = %format_args!("{error:#}"), "device sync failed");
    }
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
            } else if let Some(live) = state.voice.as_mut().filter(|live| live.channel == channel) {
                // Live file offers arrive as messages in the voice channel.
                let record = client.message(&channel, &id).await?;
                let me = *client.identity.certificate.person_id();
                if record.person != me
                    && let Some(file) = client.open_message(&channel, &record).await?.live
                {
                    live.offers.insert(file.id, record.clone());
                    ctx.send(Update::LiveOffer(LiveOffer {
                        id: file.id,
                        from: Some(record.person),
                        name: file.name,
                        size: file.size,
                    }));
                }
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
        Event::LiveFileAccepted { .. } => voice::live_file_accepted(ctx, client, state, &event)?,
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
            files,
        } => {
            let body = MessageBody {
                text,
                reply_to,
                edit_of,
                attachments: Vec::new(),
                live: None,
            };
            files::send(ctx, url, client, channel, body, files).await?;
        }
        Command::Delete(channel, id, sender, blobs) => {
            files::delete(ctx, url, client, (channel, id, sender), blobs).await?;
        }
        Command::Image(file, full) => files::image(ctx, client, file, full).await,
        Command::Save(file, path) => files::save(ctx, client, file, path).await,
        Command::OfferFile(path) => voice::offer_file(ctx, client, state, path).await?,
        Command::AcceptFile(id, path) => voice::accept_file(ctx, client, state, id, path).await?,
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
        Command::Invite(uses, seconds) => {
            let (id, link) = client.invite(uses, seconds).await?;
            ctx.send(Update::Invite(url.clone(), id, link.to_string()));
        }
        Command::Admin(request) => admin::run(ctx, url, client, state, request).await,
        Command::JoinVoice(channel) => voice::start_call(ctx, url, client, state, channel).await?,
        command if voice_only(&command) => {
            if let Some(channel) = voice_command(ctx, state, command).await? {
                send_room(ctx, url, client, state, channel);
            }
        }
        Command::ShareApp(app) => voice::share_app(ctx, url, client, state, app).await?,
        Command::ShareScreen(target, options) => voice::pick_screen(ctx, state, target, options)?,
        Command::ShareCamera(source, options) => {
            voice::share_camera(ctx, url, client, state, source, options).await?;
        }
        Command::LeaveVoice
        | Command::StopApp(_)
        | Command::StopScreen
        | Command::Watch(..)
        | Command::StopCamera
        | Command::CancelFile(_) => unreachable!("handled by voice_command"),
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
                files: Vec::new(),
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
        files: body.attachments.clone(),
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

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
