//! Network work for joined servers. It runs on a tokio runtime off the UI thread:
//! the UI sends each server task a `Command` and drains `Update`s every frame.
//! Never log names, message text or invite links here.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use eframe::egui;
use livekit::prelude::Participant;
use reqwest::{Method, StatusCode};
use rusttalk_core::{
    auth::{
        Fingerprint, InviteCode, InviteLink, KnownServer, PROTOCOL_VERSION, ProtocolError, Session,
        decode, encode,
    },
    camera::CameraOptions,
    client::{
        BlobRef, DmPin, Event, FriendSignal, KeyClient, MessageBody, MessageError, MessageRecord,
        Server as Api, invite_name,
        voice::{DataLiveFiles, LiveFileEvent, NativeVideoSource, RoomEvent, VoiceRoster},
    },
    identity::{DeviceIdentity, fingerprint},
    messages::PollPart,
    presence::{Presence, PresenceEvent, Status as Choice, Update as Pushed},
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
    server::{Body, CallState, Directory, Id, Msg, Peer, Picture, Stats, Status},
    share::App,
};

mod admin;
mod backfill;
mod calendar;
mod control;
mod directory;
mod files;
mod find;
pub mod terminal;
mod voice;

pub use admin::{Admin, AdminUpdate, AuditEntry, Ban, ImportItem, Invite, PublicListing};
pub use calendar::Change;
pub use control::{ControlView, Ended};
pub(crate) use directory::frame_image;
use directory::{directory, get};
pub use files::{MAX_FILE, file_name};
pub use find::{Listed, PAGE_SIZE};
use voice::{
    Live, Next, Transfer, leave_voice, next_voice, on_voice, room_changed, screen_changed,
    send_room, share_screen, stop_finished_shares, stop_lost_video, sync_voice_key, voice_command,
    voice_only, voice_stats,
};

const PAGE: u32 = 50;

/// What to make of a downloaded picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// The small PNG sent along with the file.
    Thumbnail,
    /// Every frame at full size, for the viewer.
    Full,
    /// Every frame shrunk to thumbnail size, to play in the chat.
    Inline,
}

// A draft is big and rare next to the other commands, which are small.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Load the newest page and follow live messages.
    Open(Id),
    Backfill(backfill::Request),
    /// Load the page before a message: channel, oldest loaded message.
    Older(Id, Id),
    Send(Draft),
    /// Channel, message, and the IDs of its stored files to remove too.
    Delete(Id, Id, Id, Vec<Id>),
    /// Check that a deleted DM copy (channel, message) is really gone here.
    CheckGone(Id, Id),
    /// Download and decode a picture, as a thumbnail, the full image or a small looping copy.
    Image(BlobRef, Fetch),
    /// Download and read a link preview file.
    LinkCard(BlobRef),
    /// Download, check and decode another server's emoji or sticker file.
    StickerFile(BlobRef, crate::sticker_files::Kind),
    /// Download a file and save it here.
    Save(BlobRef, PathBuf),
    /// Load a channel's shared files.
    SharedFiles(Id),
    /// Upload files into a channel's shared files.
    ShareFiles(Id, Vec<PathBuf>),
    /// Remove a shared file: channel, file and who shared it.
    Unshare(Id, Id, Id),
    /// Receive an offered file into this path.
    AcceptFile(Id, PathBuf),
    /// Stop our offer, or our download, of a live file.
    CancelFile(Id),
    CreateChannel {
        /// `None` creates a category named `new_category` first.
        category: Option<Id>,
        new_category: String,
        kind: crate::server::ChannelKind,
        name: String,
    },
    CreateCategory(String),
    RenameChannel {
        id: Id,
        name: String,
        topic: Option<String>,
        /// Voice channels only.
        audio_kbps: Option<u32>,
    },
    RenameCategory(Id, String),
    DeleteChannel(Id),
    DeleteCategory(Id),
    /// Save a new order: categories, each with its channels, as this person sees them.
    Reorder(Vec<(Id, Vec<Id>)>),
    SetServerName(String),
    SetDisplayName(String),
    /// Set this person's bio here; empty clears it.
    SetBio(String),
    /// Set this person's picture (PNG, JPEG or GIF bytes).
    SetAvatar(Vec<u8>),
    /// Online, away or invisible.
    SetStatus(Choice),
    /// Leave this person out of others' member lists while they appear offline.
    SetHideOffline(bool),
    /// Leave this server for good, on the server too.
    Leave,
    /// Offer a file live to whoever is online in a text channel or DM.
    SendLive(Id, PathBuf),
    OpenDm(Id),
    /// Make an invite link: uses and lifetime in seconds.
    Invite(u32, u32),
    Admin(Admin),
    /// Suggest, add, approve or delete an event.
    Calendar(Change),
    JoinVoice(Id),
    LeaveVoice,
    SyncVoiceStatus,
    ShareApp(App),
    /// Stop sharing an app, by its ID.
    StopApp(String),
    /// Share a screen or window, or open the system picker (Linux), with the sound of
    /// these apps (or all system audio) as part of the stream.
    ShareScreen(screen::Target, screen::Options, Vec<App>),
    /// Change what sound the screen share carries.
    StreamAudio(Vec<App>),
    StopScreen,
    /// Pick another screen or window while sharing, keeping the quality; no reconnect.
    ChangeScreen(screen::Target, bool),
    /// Start or stop receiving a screen or camera, by track.
    Watch(String, bool),
    /// Publish a running camera, replacing an earlier one.
    ShareCamera(NativeVideoSource, CameraOptions),
    StopCamera,
    /// Ask to control a shared screen, by track.
    RequestControl(String),
    /// Stop asking for, or controlling, a shared screen.
    ReleaseControl,
    /// Allow (`true`) or deny this request to control our screen.
    AnswerControl(rusttalk_core::control::Nonce, bool),
    /// Take control of our screen back.
    StopControl,
    /// Input for the screen we control.
    ControlInput(Vec<rusttalk_core::control::Input>),
    /// Share our terminal in the call; its output is read from here.
    ShareTerminal(crate::terminal::Shared),
    StopTerminal,
    /// Watch the terminal this device (LiveKit identity) shares, or stop watching.
    WatchTerminal(Option<String>),
    /// Allow (`true`) or deny this device's request to type in our terminal.
    AnswerTyping(String, bool),
    /// Take typing in our terminal back from this device.
    RevokeTyping(String),
    /// Ask to type in the terminal we watch, or stop asking or typing.
    AskToType,
    StopTyping,
    /// Bytes typed into the terminal we watch.
    TypeInTerminal(Vec<u8>),
}

/// A message to send, and what goes back to the composer when sending fails.
#[derive(Clone)]
pub struct Draft {
    /// Random; a DM sends the same ID to every shared server.
    pub id: Id,
    /// Unused for a DM, which this server's DM channel with `peer` gets.
    pub channel: Id,
    /// The other person, for a DM.
    pub peer: Option<Id>,
    pub text: String,
    /// The message replied to: person and ID.
    pub reply_to: Option<(Id, Id)>,
    pub edit_of: Option<Id>,
    /// An edit's revision (see `server::revision_id`); unused otherwise.
    pub revision: u64,
    /// Uploaded first, then sent with the message.
    pub files: Vec<PathBuf>,
    /// Files already stored in this channel, sent again with the message: a page
    /// keeps its pictures when it is saved as a new message.
    pub keep: Vec<rusttalk_core::client::Attachment>,
    /// A server sticker to send; `text` is its name.
    pub sticker: Option<Id>,
    /// A friend request or answer, sent with nothing else.
    pub friend: Option<(FriendSignal, u64)>,
    /// A poke: `text` is its note, sent with nothing else.
    pub poke: bool,
    /// A poll (`text` is its question), a vote or a poll's end, sent with nothing else.
    // Boxed: a poll is rare and would make every command larger.
    pub poll: Option<Box<PollPart>>,
    /// A link preview made while sending, uploaded as a hidden file (see `link_cards`).
    pub preview: Option<std::sync::Arc<crate::link_cards::Made>>,
    /// Emoji and stickers of other servers, uploaded as small files (see `sticker_files`).
    pub inline: Vec<crate::sticker_files::Inline>,
}

pub struct LiveOffer {
    pub id: Id,
    /// Who offers it; `None` for our own offer.
    pub from: Option<Id>,
    pub name: String,
    pub size: u64,
    /// Offered in a text channel or DM, rather than in the call.
    pub chat: bool,
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
    /// This person is a member there already (say, from another device): sign in, ask no name.
    pub member: bool,
}

/// How this device gets in the first time.
pub enum Admission {
    Invite(InviteCode),
    /// The server lets anyone join.
    Open,
    /// Already a member, from another device: members sign in without an invite.
    Member,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compatibility {
    ServerTooOld,
    #[default]
    Supported,
    AppTooOld,
}

impl Compatibility {
    fn for_protocol(protocol: Option<u32>) -> Self {
        const MIN_SUPPORTED_PROTOCOL: u32 = 1;
        match protocol.unwrap_or(1) {
            n if n < MIN_SUPPORTED_PROTOCOL => Self::ServerTooOld,
            n if n > PROTOCOL_VERSION => Self::AppTooOld,
            _ => Self::Supported,
        }
    }
}

pub enum Update {
    RetryDmBackfill,
    Preview(Result<Box<Preview>, String>),
    /// A page of directory results, by page number.
    Found(u32, Result<Vec<Listed>, String>),
    /// A server picked from the directory, checked; errors show in the directory.
    Listed(Result<Box<Preview>, String>),
    Status(Url, Status),
    Compatibility(Url, Compatibility),
    ChannelOrderUnavailable(Url),
    Directory(Url, Box<Directory>),
    /// How a change to the events went.
    Calendar(Url, Result<(), String>),
    Messages {
        server: Url,
        channel: Id,
        items: Vec<Msg>,
        older: bool,
        /// Set for page loads: whether this was the first page of the channel.
        at_start: Option<bool>,
    },
    Deleted(Url, Id, Id),
    /// A deleted DM copy is missing: server, DM channel and message.
    CopyGone(Url, Id, Id),
    /// Loading a page of a channel failed; `true` when its cursor is gone from the server.
    PageFailed(Url, Id, bool),
    /// A new message in any readable channel or DM, for unread marks and notifications.
    Arrived(Url, Id, Msg),
    /// The newest message time from someone else per channel, after (re)connecting.
    Latest(Url, Vec<(Id, i64)>),
    /// This person left the server; remove it here too.
    Left(Url),
    /// A member came online, went away or offline.
    Presence(Url, Id, Presence),
    Notice(Url, Id, Msg),
    DmReady(Url, Id),
    /// A new invite: its ID (to revoke it) and link.
    Invite(Url, String, String),
    Admin(Url, AdminUpdate),
    /// The call in a voice channel changed state; `None` when it ended.
    Call(Url, Id, Option<CallState>),
    /// Whether this device may talk in the call, as LiveKit currently grants it.
    CanTalk(Url, Id, bool),
    /// A moderator moved this person's call from one voice channel to another.
    VoiceMoved(Url, Id, Id),
    /// This device may not share its screen or camera in the call.
    NoVideo(Url, Id),
    /// Everyone in one voice channel.
    Room(Url, Id, Vec<Peer>, Option<crate::sounds::Snapshot>),
    CallVideoStopped(Url, Id, livekit::prelude::TrackSid),
    /// Sharing this app on its own stopped or never started.
    AppStopped(String),
    SharedAudioStopped(Url, Id),
    /// What the screen share's sound is now, after a change or a failure.
    StreamAudio(Vec<App>),
    /// This device's screen share went live (`true`), or stopped or never started.
    Sharing(bool),
    /// This device's camera stopped publishing or never started.
    CameraStopped,
    /// Remote control in the call changed.
    Control(ControlView),
    /// Asking for, or controlling, this screen (track ID) ended, and why.
    ControlEnded(String, Ended),
    /// What to show of the terminal we watch.
    Terminal(rusttalk_core::terminal::Feed),
    /// Our terminal share couldn't start.
    TerminalStopped,
    /// Who asks to type in our terminal, and who may.
    TerminalGuests(terminal::Guests),
    Stats(Url, Stats),
    /// A file upload in a channel started or went on (what to show, and how far), or ended.
    Uploading(Url, Id, Option<(String, f32)>),
    /// A channel's shared files, newest first; empty when loading them failed.
    SharedFiles(Url, Id, Vec<rusttalk_core::client::SharedFile>),
    /// How far saving a stored file got (0 to 1), by blob ID.
    SaveProgress(Id, f32),
    /// A picture's frames with their delays in ms, by blob ID, or why not: `true` when it
    /// expired or was deleted.
    Image(Id, Fetch, Result<Vec<(egui::ColorImage, u32)>, bool>),
    /// A link preview file by blob ID, or why not: `true` when it expired or was deleted.
    LinkCard(Id, Result<crate::link_cards::Card, bool>),
    /// An emoji or sticker file by blob ID, or why not: `true` when it expired or was deleted.
    StickerFile(Id, Result<Picture, bool>),
    /// A file by blob ID was saved, or why not: `true` when it expired or was deleted.
    Saved(Id, Result<(), bool>),
    /// A file offered in the voice room, by someone else or by us.
    LiveOffer(LiveOffer),
    /// Someone started (`true`) or stopped receiving our live file.
    LiveReceiver(Id, Id, bool),
    /// How receiving a live file goes.
    LiveProgress(Id, LiveFileEvent),
    /// The joined servers and synced settings after a device sync.
    Synced(Box<Synced>),
    /// Sending this message failed; put it back in the composer.
    Unsent(Url, Draft),
    Failed(String),
}

pub struct Net {
    completed_dm_copies: Arc<tokio::sync::Mutex<backfill::Completed>>,
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
            completed_dm_copies: Default::default(),
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

    /// Friends as last synced.
    pub fn contacts(&self) -> std::collections::BTreeMap<String, serde_json::Value> {
        self.ctx.sync.contacts()
    }

    /// This person's deleted DM messages as last saved, at start.
    pub fn dm_deleted(&self) -> crate::dm::Tombstones {
        self.ctx.sync.dm_deleted()
    }

    /// Check an invite's server and read its name, without joining yet. With `probe`
    /// (not joined here yet), also check whether this person is a member already.
    pub fn preview(&self, link: InviteLink, probe: bool) {
        let ctx = self.ctx.clone();
        self.spawn(async move {
            let update = preview(&ctx, &link, probe).await.map(Box::new);
            ctx.send(Update::Preview(update));
        });
    }

    /// Check a server typed in by address, without joining yet; `probe` as for `preview`.
    pub fn check_address(&self, address: Url, probe: bool) {
        let ctx = self.ctx.clone();
        self.spawn(async move {
            let update = find::address(&ctx, address, probe).await.map(Box::new);
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
    pub fn check_listed(&self, listed: Listed, probe: bool) {
        let ctx = self.ctx.clone();
        self.spawn(async move {
            let update = find::preview(&ctx, listed, probe).await.map(Box::new);
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

    pub fn remove_device(&self, record: &rusttalk_core::identity::Revocation) -> Result<()> {
        rusttalk_core::client::revocations::Revocations::in_profile(&self.ctx.data)
            .remove_other_device(&self.ctx.identity.certificate, record)?;
        self.sync(Vec::new());
        Ok(())
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

    /// Finish cancellation before sign-out removes files these tasks can write.
    pub fn stop_for_sign_out(&mut self) {
        drop(self.runtime.take());
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

async fn preview(ctx: &Ctx, link: &InviteLink, probe: bool) -> Result<Preview, String> {
    let known = Api::pinned(&ctx.data, &link.server.address).map_err(|error| {
        tracing::error!(error = %format_args!("{error:#}"), "reading server pins failed");
        t!("net.saved_servers_failed").to_owned()
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
                return Err(t!("net.invite_expired").into());
            }
        },
    };
    let member = probe && member(ctx, &api).await;
    tracing::info!(server = %fingerprint(&link.server.server_id), known = known.is_some(), open, member, "invite checked");
    Ok(Preview {
        server: link.server.clone(),
        invite,
        name,
        known: known.is_some(),
        open,
        member,
    })
}

/// Whether this person is a member of the server already, by signing in without an invite,
/// which only members may. A restored device has no other way to know, and a member
/// rejoining by invite must not be asked for a name again. The connection that follows
/// signs in afresh, which replaces this session.
async fn member(ctx: &Ctx, api: &Api) -> bool {
    match api.sign_in(&ctx.identity, None).await {
        Ok(reply) if reply.status().is_success() => true,
        Ok(reply) => {
            tracing::info!(status = %reply.status(), "not a member yet");
            false
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "checking membership failed");
            false
        }
    }
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
        t!("net.unreachable").into()
    }
}

fn mismatch(known: bool) -> String {
    if known {
        t!("net.server_changed").into()
    } else {
        t!("net.invite_mismatch").into()
    }
}

fn pin_changed(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<ProtocolError>(),
        Some(
            ProtocolError::ServerIdChanged | ProtocolError::TlsChanged | ProtocolError::ServerProof
        )
    ) || rusttalk_core::pinned_tls::is_pin_mismatch(error.as_ref())
}

#[derive(Default)]
struct State {
    dm_backfill: bool,
    /// Channels the UI has opened, which get live messages.
    loaded: HashSet<Id>,
    /// Records that waited for a channel key, retried after key work.
    waiting: HashMap<Id, HashSet<Id>>,
    last: Option<Directory>,
    order_unavailable: bool,
    /// Whether `last` was built with the space key.
    named: bool,
    display_name: Option<String>,
    notices: u64,
    server_id: Id,
    /// Who the server says is in each voice channel.
    rooms: HashMap<Id, VoiceRoster>,
    voice: Option<Live>,
    /// Live files in the call and in chats, by transfer ID.
    transfers: HashMap<Id, Transfer>,
    /// Live files in text channels and DMs, by channel: our offers, and accepting others'.
    chat_files: HashMap<Id, DataLiveFiles>,
    /// Decoded pictures in the last directory, by hash.
    pictures: HashMap<[u8; 32], Picture>,
    /// The DM pins `dm_pins` last saved, to save again when they change.
    pins_saved: HashMap<Id, DmPin>,
    /// `dm_pins` couldn't be read: it is left as it is, and DMs here stay blocked.
    pins_lost: bool,
    /// Who each voice participant (LiveKit identity) is, as last known from the API: the
    /// call's people and their videos still show while the API connection is down.
    identities: HashMap<String, Id>,
    /// DMs the server relabelled, already warned about.
    relabelled: HashSet<Id>,
    /// The voice room LiveKit last removed this device from, and when: a move notice for
    /// it may arrive just after.
    removed_from: Option<(Id, std::time::Instant)>,
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
    let records = rusttalk_core::client::revocations::Revocations::in_profile(&ctx.data);
    let mut removals = records.changes();
    let mut delay = 1;
    // Verified once, then reused: each verification spends one of the server's rate-limited challenges.
    let mut api = None;
    // Commands that came while connecting, for when it is done. Not in `State`: some
    // hold Windows handles, and `State` is borrowed across awaits so must stay `Sync`.
    let mut queued = VecDeque::new();
    loop {
        ctx.send(Update::Status(url.clone(), Status::Connecting));
        let connecting = {
            let (ctx, server, api, admission) = (&ctx, &server, &mut api, &mut admission);
            async move {
                let (api, session, dm_backfill) =
                    session(ctx, server, api, admission.as_ref()).await?;
                if admission.take().is_some() {
                    tracing::info!(server = %fingerprint(&server.server_id), "joined server");
                    let joined = vec![Edit::Join(api.known_server().clone())];
                    if let Err(error) = ctx.sync.edit(&ctx.data, joined).await {
                        tracing::error!(error = %format_args!("{error:#}"), "saving joined server failed");
                    }
                }
                let pins = load_pins(&ctx.data, &server.server_id);
                let client =
                    KeyClient::connect_with_dms(api, &ctx.identity, &session, pins.clone()).await;
                Ok::<_, Failure>((client, pins, dm_backfill))
            }
        };
        let Some(connected) = offline(
            &ctx,
            &url,
            &mut state,
            &mut commands,
            connecting,
            Some(&mut queued),
        )
        .await
        else {
            return;
        };
        match connected {
            Ok((connected, pins, dm_backfill)) => {
                state.dm_backfill = dm_backfill;
                state.pins_lost = pins.is_none();
                state.pins_saved = pins.unwrap_or_default();
                match connected {
                    Ok(mut client) => {
                        delay = 1;
                        state.order_unavailable = false;
                        tracing::info!(server = %fingerprint(&server.server_id), "connected to server");
                        ctx.send(Update::Status(url.clone(), Status::Online));
                        let end = serve(
                            &ctx,
                            &url,
                            &mut client,
                            &mut state,
                            &mut commands,
                            &mut queued,
                            &mut removals,
                        )
                        .await;
                        state.identities = voice::identities(&client, &state.server_id);
                        ctx.sync.set_peer(&url, None);
                        match end {
                            End::Quit => return,
                            End::Closed(Some(reason))
                                if matches!(reason.as_str(), "kicked" | "banned" | "revoked") =>
                            {
                                tracing::info!(%reason, "server ended membership");
                                let text = match reason.as_str() {
                                    "kicked" => t!("net.kicked"),
                                    "banned" => t!("net.banned"),
                                    _ => t!("net.device_revoked"),
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
        // What was asked while connecting can't be done now.
        for command in std::mem::take(&mut queued) {
            refuse(&ctx, &url, command);
        }
        ctx.send(Update::Status(url.clone(), Status::Offline));
        let wait = tokio::time::sleep(Duration::from_secs(delay));
        if offline(&ctx, &url, &mut state, &mut commands, wait, None)
            .await
            .is_none()
        {
            return;
        }
        delay = (delay * 2).min(30);
    }
}

/// Wait for `until` without the API connection. The voice room outlives it, so the call
/// keeps going: remote control and leaving work now (the other side may still be online),
/// and other room events wait for the connection. Other commands wait in `queued` while
/// connecting, and are refused otherwise. `None` when the window closed.
async fn offline<T>(
    ctx: &Ctx,
    url: &Url,
    state: &mut State,
    commands: &mut UnboundedReceiver<Command>,
    until: impl Future<Output = T>,
    mut queued: Option<&mut VecDeque<Command>>,
) -> Option<T> {
    tokio::pin!(until);
    // A capture that ends by itself (its window closed) still stops sharing, and control.
    let mut finished = tokio::time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            done = &mut until => return Some(done),
            next = next_voice(&mut state.voice) => voice_next(ctx, url, None, state, next).await,
            _ = finished.tick(), if state.voice.is_some() => {
                stop_finished_shares(ctx, url, None, state).await;
            }
            command = commands.recv() => match command? {
                command if voice_only(&command) => {
                    if let Some(queued) = queued.as_deref_mut() {
                        cancel_queued(queued, &command);
                    }
                    match voice_command(ctx, url, state, command).await {
                        Ok(Some(channel)) => voice::roster(ctx, url, None, state, channel),
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(error = %format_args!("{error:#}"), "voice request failed");
                        }
                    }
                }
                command => match queued.as_deref_mut() {
                    Some(queued) => queued.push_back(command),
                    None => refuse(ctx, url, command),
                },
            },
        }
    }
}

/// Leaving or stopping while connecting also drops the join or share still waiting for
/// the connection, so it doesn't start again once connected.
fn cancel_queued(queued: &mut VecDeque<Command>, command: &Command) {
    queued.retain(|queued| match (command, queued) {
        (
            Command::LeaveVoice,
            Command::JoinVoice(_)
            | Command::ShareScreen(..)
            | Command::ShareCamera(..)
            | Command::ShareApp(_),
        ) => false,
        (Command::StopScreen, Command::ShareScreen(..)) => false,
        (Command::StopCamera, Command::ShareCamera(..)) => false,
        (Command::StopApp(id), Command::ShareApp(app)) => app.id != *id,
        _ => true,
    });
}

/// Turn down a command that needs the API connection, which is down.
fn refuse(ctx: &Ctx, url: &Url, command: Command) {
    match command {
        // The status and hiding are sent again once connected, when they differ.
        // So is a pending bio.
        Command::SetStatus(_)
        | Command::SetHideOffline(_)
        | Command::SetBio(_)
        | Command::CheckGone(..)
        | Command::Backfill(_) => {}
        Command::ShareApp(app) => {
            ctx.send(Update::AppStopped(app.id));
            ctx.send(Update::Failed(t!("admin.offline").into()));
        }
        Command::ShareScreen(..) => {
            ctx.send(Update::Sharing(false));
            ctx.send(Update::Failed(t!("admin.offline").into()));
        }
        Command::ShareCamera(..) => {
            ctx.send(Update::CameraStopped);
            ctx.send(Update::Failed(t!("admin.offline").into()));
        }
        Command::Admin(_) => {
            ctx.send(Update::Admin(
                url.clone(),
                AdminUpdate::Failed(t!("admin.offline").into()),
            ));
        }
        Command::Calendar(_) => ctx.send(Update::Calendar(
            url.clone(),
            Err(t!("admin.offline").into()),
        )),
        Command::Send(draft) => {
            ctx.send(Update::Unsent(url.clone(), draft));
            ctx.send(Update::Failed(t!("admin.offline").into()));
        }
        Command::Image(file, fetch) => ctx.send(Update::Image(file.id, fetch, Err(false))),
        Command::Save(file, _) => ctx.send(Update::Saved(file.id, Err(false))),
        Command::SharedFiles(channel) => {
            ctx.send(Update::SharedFiles(url.clone(), channel, Vec::new()));
            ctx.send(Update::Failed(t!("admin.offline").into()));
        }
        Command::AcceptFile(id, _) => {
            let reason = t!("admin.offline").into();
            ctx.send(Update::LiveProgress(id, LiveFileEvent::Failed { reason }));
        }
        command => {
            if let Command::JoinVoice(channel) = command {
                ctx.send(Update::Call(url.clone(), channel, None));
            }
            ctx.send(Update::Failed(t!("admin.offline").into()));
        }
    }
}

async fn session<'a>(
    ctx: &Ctx,
    server: &KnownServer,
    api: &'a mut Option<Api>,
    admission: Option<&Admission>,
) -> Result<(&'a Api, Session, bool), Failure> {
    let stop_or_retry = |error: anyhow::Error| {
        if pin_changed(&error) {
            tracing::error!(error = %format_args!("{error:#}"), "server pins differ");
            Failure::Stop(t!("net.server_changed").into())
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
    let api = api.as_mut().expect("set above");
    let mut dm_backfill = false;
    let protocol = match api.info().await {
        Ok(info) => {
            dm_backfill = info.dm_backfill;
            Some(info.protocol)
        }
        Err(error) if pin_changed(&error) => return Err(stop_or_retry(error)),
        Err(error)
            if error
                .downcast_ref::<reqwest::Error>()
                .and_then(reqwest::Error::status)
                == Some(StatusCode::NOT_FOUND) =>
        {
            Some(None)
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "reading server version failed");
            None
        }
    };
    if let Some(protocol) = protocol {
        let compatibility = Compatibility::for_protocol(protocol);
        ctx.send(Update::Compatibility(server.address.clone(), compatibility));
        if compatibility == Compatibility::AppTooOld {
            tracing::warn!(?protocol, "server requires a newer protocol");
            return Err(Failure::Stop(t!("net.needs_newer").into()));
        }
    }
    let reply = match admission {
        Some(Admission::Open) => api.join_open(&ctx.identity).await,
        Some(Admission::Invite(code)) => api.sign_in(&ctx.identity, Some(code)).await,
        Some(Admission::Member) | None => api.sign_in(&ctx.identity, None).await,
    }
    .map_err(stop_or_retry)?;
    let status = reply.status();
    if status.is_success() {
        let session = reply
            .json()
            .await
            .map_err(|error| stop_or_retry(error.without_url().into()))?;
        // Refresh only at sign-in. Missing server info keeps this session on v2 envelopes.
        api.protocol = protocol.flatten().unwrap_or(1);
        return Ok((api, session, dm_backfill));
    }
    let code = reply
        .json::<Value>()
        .await
        .ok()
        .and_then(|body| body["error"].as_str().map(str::to_owned))
        .unwrap_or_default();
    tracing::warn!(%status, code, "sign-in refused");
    Err(match code.as_str() {
        "invite_required" if matches!(admission, None | Some(Admission::Member)) => {
            Failure::Stop(t!("net.not_member").into())
        }
        "invite_required" | "invalid_invite" | "invite_not_found" => {
            Failure::Stop(t!("net.invite_expired").into())
        }
        "person_banned" | "ip_banned" => Failure::Stop(t!("net.banned").into()),
        "open_joining_disabled" => Failure::Stop(t!("net.needs_invite").into()),
        "member_cap_reached" => Failure::Stop(t!("net.full").into()),
        "open_join_limit" => Failure::Stop(t!("net.join_limit").into()),
        "client_update_required" => {
            ctx.send(Update::Compatibility(
                server.address.clone(),
                Compatibility::AppTooOld,
            ));
            Failure::Stop(t!("net.needs_newer").into())
        }
        "device_revoked" => Failure::Stop(t!("net.device_revoked").into()),
        "device_limit" => Failure::Stop(t!("net.device_limit").into()),
        _ if status.is_server_error()
            || status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::UNAUTHORIZED =>
        {
            Failure::Retry
        }
        _ => Failure::Stop(t!("net.sign_in_failed").into()),
    })
}

async fn serve(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    commands: &mut UnboundedReceiver<Command>,
    queued: &mut VecDeque<Command>,
    removals: &mut tokio::sync::watch::Receiver<()>,
) -> End {
    if let Err(error) = resume(ctx, url, client, state).await {
        tracing::warn!(error = %format_args!("{error:#}"), "loading server state failed");
        return End::Error;
    }
    voice::name_audio(ctx, client, state);
    // What was asked while connecting.
    for command in std::mem::take(queued) {
        run_command(ctx, url, client, state, command).await;
    }
    let mut stats = tokio::time::interval(Duration::from_secs(2));
    loop {
        // Refresh ten minutes before the 12-hour token expires.
        let refresh = Duration::from_secs((client.expires_at() - 600 - now()).max(0) as u64);
        // A DM epoch that isn't trusted yet is tried again then, also without an event.
        let rekey = client.next_rekey();
        let rekey_due = async move {
            match rekey {
                Some(at) => tokio::time::sleep_until(at.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = removals.changed() => {
                if let Err(error) = client.sync().await {
                    tracing::warn!(%error, "could not rotate after device removal");
                    sync_voice_key(ctx, client, state);
                    continue;
                }
                sync_voice_key(ctx, client, state);
                save_pins(ctx, url, client, state);
            }
            event = client.next_update() => match event {
                Ok(Some(Pushed::Presence(PresenceEvent::PresenceChanged { person_id, status }))) => {
                    match decode(&person_id) {
                        Ok(person) => ctx.send(Update::Presence(url.clone(), person, status)),
                        Err(error) => tracing::warn!(%error, "presence for a bad person ID"),
                    }
                }
                Ok(Some(Pushed::Event(event))) => {
                    if let Err(error) = on_event(ctx, url, client, state, event).await {
                        tracing::warn!(error = %format_args!("{error:#}"), "handling a server event failed");
                    }
                    sync_voice_key(ctx, client, state);
                }
                Ok(None) => return End::Closed(client.close_reason.clone()),
                Err(error) => {
                    sync_voice_key(ctx, client, state);
                    tracing::warn!(error = %format_args!("{error:#}"), "event stream failed");
                    return End::Error;
                }
            },
            command = commands.recv() => {
                let Some(command) = command else {
                    return End::Quit;
                };
                run_command(ctx, url, client, state, command).await;
            }
            () = rekey_due => {
                match client.sync().await {
                    Ok(()) => { sync_voice_key(ctx, client, state); save_pins(ctx, url, client, state); },
                    Err(error) => {
                        tracing::warn!(error = %format_args!("{error:#}"), "DM key recovery failed");
                        // Wait before the next try, so a fast failure can't spin.
                        client.rekey_failed();
                    }
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
            next = next_voice(&mut state.voice) => voice_next(ctx, url, Some(client), state, next).await,
            _ = stats.tick(), if state.voice.is_some() => {
                voice::sync_voice_status(ctx, state).await;
                if let Some(live) = &mut state.voice {
                    voice::sync_video_watches(live).await;
                }
                stop_finished_shares(ctx, url, Some(client), state).await;
                voice::name_audio(ctx, client, state);
                voice_stats(ctx, url, client, state).await;
            }
        }
    }
}

async fn run_command(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    command: Command,
) {
    if let Err(error) = on_command(ctx, url, client, state, command).await {
        tracing::warn!(error = %format_args!("{error:#}"), "server request failed");
        let text = match error.downcast_ref::<MessageError>() {
            Some(MessageError::ServerUpdateRequired) => t!("net.rotation_update"),
            Some(MessageError::TextLimit) => t!("net.text_limit"),
            _ => t!("admin.failed"),
        };
        ctx.send(Update::Failed(text.into()));
    }
}

/// A voice room event or finished screen pick. Without `client` the API connection is
/// down; the call goes on with the people last known.
async fn voice_next(
    ctx: &Ctx,
    url: &Url,
    client: Option<&KeyClient<'_>>,
    state: &mut State,
    next: Next,
) {
    match next {
        Next::Event(Some(RoomEvent::DataReceived {
            payload,
            participant: Some(participant),
            topic,
            ..
        })) => {
            if topic.as_deref() == Some(rusttalk_core::client::voice::terminal::TOPIC) {
                voice::terminal_received(ctx, client, state, &participant, &payload).await;
            } else {
                voice::control_received(ctx, client, state, &participant, &payload).await;
            }
        }
        Next::Flush => {
            if let Some(live) = &mut state.voice {
                terminal::flush(live).await;
            }
        }
        Next::Event(event) => {
            if let Some(RoomEvent::ParticipantPermissionChanged {
                participant: Participant::Local(_),
                permission,
            }) = &event
                && let Some(live) = &mut state.voice
            {
                let denied = live
                    .room
                    .stop_denied_shared_audio(permission.as_ref())
                    .await;
                if denied {
                    live.stop_audio_captures();
                    ctx.send(Update::SharedAudioStopped(url.clone(), live.channel));
                }
                if let Err(error) = live
                    .room
                    .sync_microphone(permission.as_ref(), ctx.audio.source(), || {
                        ctx.audio.muted() || ctx.audio.deafened()
                    })
                    .await
                {
                    tracing::warn!(error = %format_args!("{error:#}"), "updating microphone publication failed");
                }
                // Permission can return before microphone publication succeeds.
                ctx.send(Update::CanTalk(
                    url.clone(),
                    live.channel,
                    live.room.can_publish() && live.room.has_microphone(),
                ));
                if denied {
                    let channel = live.channel;
                    voice::roster(ctx, url, client, state, channel);
                }
                stop_lost_video(ctx, url, client, state, None).await;
            } else if let Some(RoomEvent::LocalTrackUnpublished { publication, .. }) = &event {
                stop_lost_video(ctx, url, client, state, Some(publication.sid())).await;
            }
            on_voice(ctx, url, client, state, event).await;
        }
        Next::Capture(capture) => match client {
            Some(client) => share_screen(ctx, url, client, state, capture).await,
            // Like a new share while offline: publishing needs the API.
            None => {
                if let Some(live) = &mut state.voice {
                    live.starting = None;
                }
                ctx.send(Update::Sharing(false));
                ctx.send(Update::Failed(t!("admin.offline").into()));
            }
        },
        Next::Swap(capture) => screen_changed(ctx, state, capture),
        Next::App(app, stream, started) => {
            voice::app_started(ctx, url, client, state, app, stream, started).await;
        }
    }
}

/// Whether a move notice from `from` is for this device: its own call there, or the room
/// LiveKit just removed it from (the server sends the notice right after the removal), so a
/// notice can't start a call on an idle device.
fn move_applies(
    call: Option<Id>,
    removed: Option<(Id, std::time::Instant)>,
    from: Id,
    now: std::time::Instant,
) -> bool {
    call == Some(from)
        || removed.is_some_and(|(channel, at)| {
            channel == from && now.duration_since(at) < std::time::Duration::from_secs(10)
        })
}

/// Reload what the UI shows after (re)connecting, since events may have been missed.
async fn resume(ctx: &Ctx, url: &Url, client: &mut KeyClient<'_>, state: &mut State) -> Result<()> {
    // Sync first, beside the rest: on a device restored from the words this first server
    // brings back the others, which must not wait for (or be skipped by a failure in) the
    // directory, history and voice loads below. Also after each token refresh, which picks
    // up changes from other devices twice a day.
    let (api, session) = client.sync_peer();
    ctx.sync.set_peer(url, Some((api.clone(), session.clone())));
    let sync_ctx = ctx.clone();
    tokio::spawn(async move { sync(&sync_ctx, Vec::new()).await });
    directory(ctx, url, client, state).await?;
    for channel in state.loaded.clone() {
        page(ctx, url, client, state, channel, None).await;
    }
    set_display_name(ctx, url, client, state).await;
    latest(ctx, url, client, state).await;
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
    sync_voice_key(ctx, client, state);
    Ok(())
}

/// The newest message from someone else in every channel and DM, so what came in
/// while away shows as unread.
// shortcut: one request per channel on each (re)connect; the newest record may be an
// edit, which then also counts as new.
async fn latest(ctx: &Ctx, url: &Url, client: &mut KeyClient<'_>, state: &State) {
    let Some(dir) = &state.last else {
        return;
    };
    // A DM's newest record counts only once it opens and comes from its pinned peer:
    // the server could put anything there.
    let channels = (dir.categories.iter())
        .flat_map(|category| &category.channels)
        .map(|channel| (channel.id, None))
        .chain(dir.dms.iter().map(|dm| (dm.channel, Some(dm.peer))));
    let me = *client.identity.certificate.person_id();
    let mut found = Vec::new();
    for (channel, peer) in channels {
        match client.message_page(&channel, None, 1).await {
            Ok(records) => {
                let Some(record) = records.first().filter(|record| record.person != me) else {
                    continue;
                };
                if let Some(peer) = peer
                    && (record.backfilled
                        || record.person != peer
                        || !(client.open_message(&channel, record).await)
                            .is_ok_and(|body| body.key_link.is_none() && !body.poke))
                {
                    continue;
                }
                found.push((channel, record.at));
            }
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "loading the newest message failed");
            }
        }
    }
    ctx.send(Update::Latest(url.clone(), found));
}

/// Merge with this person's other devices and show the result.
async fn sync(ctx: &Ctx, edits: Vec<Edit>) {
    let result = ctx
        .sync
        .run(&ctx.data, &ctx.identity, edits, |synced| {
            for address in &synced.conflicts {
                let server = address.host_str().unwrap_or(t!("net.a_server"));
                ctx.send(Update::Failed(t!("net.sync_conflict", server = server)));
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
    if client.take_own_revocations_changed() {
        sync(ctx, Vec::new()).await;
    }
    match event {
        Event::MessageCreated { channel, id } => {
            let (channel, id) = (decode(&channel)?, decode(&id)?);
            let record = client.message(&channel, &id).await?;
            if state.loaded.contains(&channel) {
                let Some(msg) = open(client, state, &channel, &record).await else {
                    return Ok(());
                };
                ctx.send(Update::Arrived(url.clone(), channel, msg.clone()));
                send_messages(ctx, url, channel, vec![msg], false, None);
                return Ok(());
            }
            // Not opened: decrypt only for the unread mark and notification, keeping no state.
            let body = client.open_message(&channel, &record).await;
            if let Err(error) = &body {
                tracing::warn!(error = %format_args!("{error:#}"), "message could not be opened");
            }
            let msg = match &body {
                Ok(body) if body.key_link.is_some() => return Ok(()),
                Ok(body) => to_msg(&record, body),
                Err(_) => unreadable(&record),
            };
            ctx.send(Update::Arrived(url.clone(), channel, msg));
            let me = *client.identity.certificate.person_id();
            // Live file offers arrive as messages in the voice channel.
            if let Some(live) = state.voice.as_mut().filter(|live| live.channel == channel)
                && record.person != me
                && let Ok(Some(file)) = body.map(|body| body.live)
            {
                let channel = live.channel;
                let transfer = Transfer {
                    channel,
                    offer: Some(record.clone()),
                    ..Transfer::default()
                };
                state.transfers.insert(file.id, transfer);
                ctx.send(Update::LiveOffer(LiveOffer {
                    id: file.id,
                    from: Some(record.person),
                    name: file.name,
                    size: file.size,
                    chat: false,
                }));
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
            // Key work may have re-keyed a DM.
            save_pins(ctx, url, client, state);
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
                    items.extend(open(client, state, &channel, &record).await);
                }
                send_messages(ctx, url, channel, items, false, None);
            }
            set_display_name(ctx, url, client, state).await;
        }
        Event::VoiceChanged { channel } => {
            room_changed(ctx, url, client, state, decode(&channel)?).await?;
        }
        Event::VoiceMoved { from, to, .. } => {
            let (from, to) = (decode(&from)?, decode(&to)?);
            let call = state.voice.as_ref().map(|live| live.channel);
            let removed = state.removed_from.take();
            if move_applies(call, removed, from, std::time::Instant::now()) {
                ctx.send(Update::VoiceMoved(url.clone(), from, to));
            } else {
                tracing::warn!("ignored a voice move for a call this device is not in");
            }
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
        Command::Send(draft) => {
            let channel = match draft.peer {
                Some(peer) => dm_channel(ctx, url, client, state, peer).await,
                None => Ok(draft.channel),
            };
            let channel = match channel {
                Ok(channel) => channel,
                Err(error) => {
                    ctx.send(Update::Unsent(url.clone(), draft));
                    return Err(error);
                }
            };
            // Only to a DM that just the two people can read: pinned to this person,
            // with a key from one of their devices. A missing key fails below as before.
            let scope = rusttalk_core::keys::Scope {
                kind: rusttalk_core::keys::Kind::Channel,
                id: channel,
            };
            if let Some(peer) = draft.peer
                && client.current_key(scope).is_ok()
                && !client.dm_private(&channel, &peer)
                && !recovered(ctx, url, client, state, &channel, &peer).await
            {
                tracing::warn!("not sending a DM where someone else could read it");
                let server = url.host_str().unwrap_or(t!("net.a_server"));
                ctx.send(Update::Unsent(url.clone(), draft));
                ctx.send(Update::Failed(t!("net.dm_not_private", server = server)));
                return Ok(());
            }
            let mut body = MessageBody {
                text: draft.text.clone(),
                // Edits carry their revision there; current apps ignore it on edits.
                reply_to: match draft.edit_of {
                    Some(_) => Some(crate::server::revision_id(draft.revision)),
                    None => draft.reply_to.map(|(_, id)| id),
                },
                quoted_author: draft
                    .reply_to
                    .filter(|_| draft.edit_of.is_none())
                    .map(|(person, _)| person),
                edit_of: draft.edit_of,
                attachments: draft.keep.clone(),
                live: None,
                sticker: draft.sticker,
                friend: draft.friend.map(|(signal, _)| signal),
                friend_sent: draft.friend.map_or(0, |(_, sent)| sent),
                key_link: None,
                poke: draft.poke,
                poll: draft.poll.as_deref().cloned(),
            };
            if draft.peer.is_some()
                && let Some(dir) = &state.last
            {
                // Emoji of other servers carry their pictures as files and keep their tokens.
                let kept: Vec<&str> = (draft.inline.iter())
                    .filter(|picture| picture.kind == crate::sticker_files::Kind::Emoji)
                    .map(|picture| picture.name.as_str())
                    .collect();
                crate::emoji::for_replica_keeping(
                    &mut body.text,
                    &mut body.sticker,
                    &dir.emoji,
                    &kept,
                );
            }
            let files = (
                draft.files.clone(),
                draft.preview.clone(),
                draft.inline.clone(),
            );
            // A page version names its stored files, so the server keeps them (see `post_page`).
            let page = (state.last.iter())
                .flat_map(|dir| &dir.categories)
                .flat_map(|category| &category.channels)
                .any(|known| known.id == channel && known.page);
            let target = (channel, draft.id, page);
            let mut sent = files::send(ctx, url, client, target, body.clone(), files.clone()).await;
            // The other person re-keyed the DM just now: take the new key and send
            // once more, whatever this was (text, sticker, file or friend message).
            if draft.peer.is_some() && sent.as_ref().is_err_and(files::conflict) {
                // A failed key sync still puts the draft back below.
                sent = match client.sync().await {
                    Ok(()) => files::send(ctx, url, client, target, body, files).await,
                    Err(error) => Err(error),
                };
            }
            if !matches!(sent, Ok(true)) {
                ctx.send(Update::Unsent(url.clone(), draft));
            }
            sent?;
        }
        Command::CheckGone(channel, id) => {
            // The server's word that it deleted it isn't enough; it must be missing.
            if let Err(error) = client.message(&channel, &id).await
                && files::gone(&error)
            {
                ctx.send(Update::CopyGone(url.clone(), channel, id));
            }
        }
        Command::Delete(channel, id, sender, blobs) => {
            files::delete(ctx, url, client, (channel, id, sender), blobs).await?;
        }
        Command::Image(file, fetch) => files::image(ctx, client, file, fetch).await,
        Command::LinkCard(file) => files::link_card(ctx, client, file).await,
        Command::StickerFile(file, kind) => files::sticker_file(ctx, client, file, kind).await,
        Command::Save(file, path) => files::save(ctx, client, file, path).await,
        Command::SharedFiles(channel) => files::shared(ctx, url, client, channel).await,
        Command::ShareFiles(channel, paths) => files::share(ctx, url, client, channel, paths).await,
        Command::Unshare(channel, id, uploader) => {
            files::unshare(ctx, url, client, (channel, id, uploader)).await;
        }
        Command::SendLive(channel, path) => {
            voice::offer_chat_file(ctx, url, client, state, channel, path).await?;
        }
        Command::AcceptFile(id, path) if state.transfers.get(&id).is_some_and(|t| t.chat) => {
            voice::accept_chat_file(ctx, client, state, id, path).await?;
        }
        Command::AcceptFile(id, path) => voice::accept_file(ctx, client, state, id, path).await?,
        Command::CreateChannel {
            category,
            new_category,
            kind,
            name,
        } => {
            let category = match category {
                Some(id) => id,
                None => client.create_category(new_category.as_bytes()).await?,
            };
            client
                .create_channel(&category, kind.wire(), name.as_bytes(), b"")
                .await?;
            directory(ctx, url, client, state).await?;
        }
        Command::CreateCategory(name) => {
            client.create_category(name.as_bytes()).await?;
            directory(ctx, url, client, state).await?;
        }
        Command::RenameChannel {
            id,
            name,
            topic,
            audio_kbps,
        } => {
            directory::rename_channel(client, &id, &name, topic.as_deref(), audio_kbps).await?;
            directory(ctx, url, client, state).await?;
        }
        Command::RenameCategory(id, name) => {
            directory::rename_category(client, &id, &name).await?;
            directory(ctx, url, client, state).await?;
        }
        Command::DeleteChannel(id) => {
            client
                .request(Method::DELETE, &format!("/v1/channels/{}", encode(&id)))
                .send()
                .await?
                .error_for_status()?;
            directory(ctx, url, client, state).await?;
        }
        Command::DeleteCategory(id) => {
            let reply = client
                .request(Method::DELETE, &format!("/v1/categories/{}", encode(&id)))
                .send()
                .await?;
            if reply.status() == StatusCode::CONFLICT {
                tracing::warn!(category = %encode(&id), "category still contains channels");
                ctx.send(Update::Failed(t!("net.category_not_empty").into()));
                return Ok(());
            }
            reply.error_for_status()?;
            directory(ctx, url, client, state).await?;
        }
        Command::Reorder(order) => {
            if !state.order_unavailable {
                match directory::save_order(client, &order).await {
                    Ok(true) => return Ok(()),
                    Ok(false) => {
                        tracing::info!("server does not support channel ordering");
                        state.order_unavailable = true;
                        ctx.send(Update::ChannelOrderUnavailable(url.clone()));
                        ctx.send(Update::Failed(t!("net.reorder_unsupported").into()));
                    }
                    Err(error) => {
                        // Back to the server's order before the error shows.
                        directory(ctx, url, client, state).await?;
                        return Err(error);
                    }
                }
            }
            if let Some(dir) = &state.last {
                ctx.send(Update::Directory(url.clone(), Box::new(dir.clone())));
            }
        }
        Command::SetServerName(name) => {
            client.set_server_name(name.as_bytes()).await?;
            directory(ctx, url, client, state).await?;
        }
        Command::SetDisplayName(name) => {
            state.display_name = Some(name);
            set_display_name(ctx, url, client, state).await;
        }
        // Without the space key yet, the bio waits: it stays pending and is sent on a later load.
        Command::SetBio(bio) if client.has_space_key() => {
            if let Err(error) = client.set_bio(&bio).await {
                tracing::warn!(error = %format_args!("{error:#}"), "setting bio failed");
                ctx.send(Update::Failed(t!("net.bio_failed").into()));
                return Ok(());
            }
            directory(ctx, url, client, state).await?;
        }
        Command::SetBio(_) => {}
        Command::SetAvatar(bytes) => {
            if let Err(error) = client.set_avatar(&bytes).await {
                tracing::warn!(error = %format_args!("{error:#}"), "setting picture failed");
                ctx.send(Update::Failed(t!("net.picture_failed").into()));
                return Ok(());
            }
            directory(ctx, url, client, state).await?;
        }
        Command::SetStatus(choice) => client.set_status(choice).await?,
        Command::SetHideOffline(hide) => client.set_hide_offline(hide).await?,
        Command::Leave => {
            let reply = client
                .request(Method::DELETE, "/v1/me/membership")
                .send()
                .await?;
            if reply.status() == StatusCode::CONFLICT {
                ctx.send(Update::Failed(t!("net.owner_leave").into()));
                return Ok(());
            }
            reply.error_for_status()?;
            tracing::info!(server = %fingerprint(&state.server_id), "left server");
            ctx.send(Update::Left(url.clone()));
        }
        Command::Backfill(request) => request.run(ctx, url, client, state).await,
        Command::OpenDm(peer) => {
            let channel = dm_channel(ctx, url, client, state, peer).await?;
            ctx.send(Update::DmReady(url.clone(), channel));
        }
        Command::Invite(uses, seconds) => {
            let (id, link) = client.invite(uses, seconds).await?;
            ctx.send(Update::Invite(url.clone(), id, link.to_string()));
        }
        Command::Admin(request) => admin::run(ctx, url, client, state, request).await,
        Command::Calendar(change) => calendar::change(ctx, url, client, change).await,
        Command::JoinVoice(channel) => voice::start_call(ctx, url, client, state, channel).await?,
        command if voice_only(&command) => {
            if let Some(channel) = voice_command(ctx, url, state, command).await? {
                send_room(ctx, url, client, state, channel);
            }
        }
        Command::ShareApp(app) => voice::share_app(ctx, state, app),
        Command::ShareScreen(target, options, audio) => {
            voice::pick_screen(ctx, state, target, options, audio)?;
        }
        Command::ShareCamera(source, options) => {
            voice::share_camera(ctx, url, client, state, source, options).await?;
        }
        Command::LeaveVoice
        | Command::SyncVoiceStatus
        | Command::StopApp(_)
        | Command::StopScreen
        | Command::StreamAudio(_)
        | Command::ChangeScreen(..)
        | Command::Watch(..)
        | Command::StopCamera
        | Command::CancelFile(_)
        | Command::RequestControl(_)
        | Command::ReleaseControl
        | Command::AnswerControl(..)
        | Command::StopControl
        | Command::ControlInput(_)
        | Command::ShareTerminal(_)
        | Command::StopTerminal
        | Command::WatchTerminal(_)
        | Command::AnswerTyping(..)
        | Command::RevokeTyping(_)
        | Command::AskToType
        | Command::StopTyping
        | Command::TypeInTerminal(_) => unreachable!("handled by voice_command"),
    }
    Ok(())
}

/// This server's DM channel with a person, made when there is none yet.
async fn dm_channel(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    peer: Id,
) -> Result<Id> {
    let existing = (state.last.iter())
        .flat_map(|dir| &dir.dms)
        .find(|dm| dm.peer == peer);
    if let Some(dm) = existing {
        return Ok(dm.channel);
    }
    // Pinned there, before any key is wrapped.
    let scope = client.create_dm(&peer).await?;
    directory(ctx, url, client, state).await?;
    Ok(scope.id)
}

/// A DM message waits on an epoch that isn't trusted yet: try to recover it now
/// instead of at the next scheduled attempt. Whether the DM is private afterwards.
async fn recovered(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: &Id,
    peer: &Id,
) -> bool {
    client.rekey_now();
    if let Err(error) = client.sync().await {
        tracing::warn!(error = %format_args!("{error:#}"), "DM key recovery failed");
        return false;
    }
    save_pins(ctx, url, client, state);
    client.dm_private(channel, peer)
}

/// `dm_pins/<server ID>.json`: each DM channel on this server and who it is with, so
/// the server can't later relabel a DM as being with someone else.
fn pins_path(data: &Path, server_id: &Id) -> PathBuf {
    data.join("dm_pins")
        .join(format!("{}.json", hex::encode(server_id)))
}

/// A pin as saved: the person, and the newest epoch known to be the pair's, with
/// its commitment.
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedPin {
    peer: String,
    #[serde(default)]
    anchor: Option<(i64, String)>,
}

/// `None` when the file is there but can't be read: trusting the next catalog again
/// would let the server relabel DMs, so they stay blocked instead.
fn load_pins(data: &Path, server_id: &Id) -> Option<HashMap<Id, DmPin>> {
    let saved: HashMap<String, SavedPin> = match crate::load_json(&pins_path(data, server_id)) {
        Ok(saved) => saved,
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "DM pins unreadable");
            return None;
        }
    };
    (saved.iter())
        .map(|(channel, pin)| {
            let peer = decode(&pin.peer).ok()?;
            let anchor = match &pin.anchor {
                Some((epoch, commitment)) => Some((*epoch, decode(commitment).ok()?)),
                None => None,
            };
            Some((decode(channel).ok()?, DmPin::With { peer, anchor }))
        })
        .collect()
}

/// Save the DM pins when they changed. Never over a file that couldn't be read.
fn save_pins(ctx: &Ctx, url: &Url, client: &mut KeyClient<'_>, state: &mut State) {
    if state.pins_lost || *client.dms() == state.pins_saved {
        return;
    }
    let path = pins_path(&ctx.data, &state.server_id);
    let pins: HashMap<String, SavedPin> = (client.dms().iter())
        .filter_map(|(channel, pin)| match pin {
            DmPin::With { peer, anchor } => Some((
                encode(channel),
                SavedPin {
                    peer: encode(peer),
                    anchor: anchor.map(|(epoch, commitment)| (epoch, encode(&commitment))),
                },
            )),
            DmPin::Blocked => None,
        })
        .collect();
    let saved = std::fs::create_dir_all(path.parent().expect("in a folder"))
        .map_err(anyhow::Error::from)
        .and_then(|()| rusttalk_core::client::save_json(&ctx.data, &path, &pins));
    match saved {
        Ok(()) => state.pins_saved = client.dms().clone(),
        // A pin that isn't kept would be trusted afresh next time: pause DMs here.
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "saving DM pins failed");
            client.pause_dms();
            state.pins_lost = true;
            let server = url.host_str().unwrap_or(t!("net.a_server"));
            ctx.send(Update::Failed(t!("net.dm_pins_unsaved", server = server)));
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
            // A channel stays loading: reconnecting reloads opened channels, and paging
            // stays paused until then. A DM keeps paging its other copies.
            let missing = (error.downcast_ref::<reqwest::Error>()).and_then(reqwest::Error::status)
                == Some(StatusCode::NOT_FOUND);
            ctx.send(Update::PageFailed(url.clone(), channel, missing));
            ctx.send(Update::Failed(t!("net.messages_failed").into()));
            return;
        }
    };
    let at_start = records.len() < PAGE as usize;
    let mut items = Vec::with_capacity(records.len());
    for record in records.iter().rev() {
        items.extend(open(client, state, &channel, record).await);
    }
    send_messages(ctx, url, channel, items, before.is_some(), Some(at_start));
}

async fn open(
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: &Id,
    record: &MessageRecord,
) -> Option<Msg> {
    match client.open_message(channel, record).await {
        // Key work only (the client noted it); not a message to show.
        Ok(body) if body.key_link.is_some() => None,
        Ok(body) => {
            if let Some(file) = &body.live {
                let transfer = state.transfers.entry(file.id);
                transfer.or_insert(Transfer::chat(*channel)).offer = Some(record.clone());
            }
            Some(to_msg(record, &body))
        }
        Err(error) => {
            if error.downcast_ref::<MessageError>() == Some(&MessageError::MissingKey) {
                state.waiting.entry(*channel).or_default().insert(record.id);
            }
            tracing::warn!(error = %format_args!("{error:#}"), "message could not be opened");
            Some(unreadable(record))
        }
    }
}

fn unreadable(record: &MessageRecord) -> Msg {
    Msg {
        backfilled: record.backfilled,
        id: record.id,
        person: record.person,
        at: record.at,
        reply_to: None,
        quoted_author: None,
        edit_of: None,
        body: Body::Unreadable,
        files: Vec::new(),
        live: None,
        sticker: None,
        replicas: Vec::new(),
        copy_files: Vec::new(),
    }
}

fn to_msg(record: &MessageRecord, body: &MessageBody) -> Msg {
    Msg {
        backfilled: record.backfilled,
        id: record.id,
        person: record.person,
        at: record.at,
        reply_to: body.reply_to,
        quoted_author: body.quoted_author,
        edit_of: body.edit_of,
        body: match body.friend {
            Some(signal) => Body::Friend(signal, body.friend_sent),
            None if body.poke => Body::Poke(body.text.clone()),
            None => match &body.poll {
                Some(PollPart::Open(poll)) => Body::Poll(body.text.clone(), poll.clone()),
                Some(part) => Body::PollPart(part.clone()),
                None => Body::Text(body.text.clone()),
            },
        },
        files: body.attachments.clone(),
        live: (body.live.as_ref()).map(|file| (file.id, file.name.clone(), file.size)),
        sticker: body.sticker,
        replicas: Vec::new(),
        copy_files: Vec::new(),
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
            ctx.send(Update::Failed(t!("net.name_failed").into()));
        }
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod protocol_tests {
    use super::*;

    #[test]
    fn a_move_counts_only_for_this_devices_call_or_its_fresh_removal() {
        use std::time::{Duration, Instant};
        let (from, other) = ([1; 32], [2; 32]);
        let now = Instant::now();
        // The server removes first, so the notice can be handled right after the removal.
        assert!(move_applies(None, Some((from, now)), from, now));
        // Or before it, while the call is still there.
        assert!(move_applies(Some(from), None, from, now));
        // Not on an idle device, another call, another room's removal, or much later.
        assert!(!move_applies(None, None, from, now));
        assert!(!move_applies(Some(other), None, from, now));
        assert!(!move_applies(None, Some((other, now)), from, now));
        let later = now + Duration::from_secs(11);
        assert!(!move_applies(None, Some((from, now)), from, later));
    }

    #[test]
    fn a_friend_message_keeps_its_signed_send_time() {
        let record = MessageRecord {
            backfilled: false,
            id: [1; 32],
            epoch: 1,
            person: [2; 32],
            sender_device: [3; 32],
            at: 5,
            size: 0,
            envelope: String::new(),
        };
        let body = MessageBody {
            friend: Some(FriendSignal::Request),
            friend_sent: 9_000,
            ..MessageBody::default()
        };
        // The server's time stays for the chat; friend state uses the signed one.
        let msg = to_msg(&record, &body);
        assert!(msg.body == Body::Friend(FriendSignal::Request, 9_000) && msg.at == 5);
    }

    #[test]
    fn checks_server_protocol_against_supported_range() {
        let old_info: rusttalk_core::auth::ServerInfo =
            serde_json::from_str(r#"{"open_joining":true}"#).unwrap();
        assert_eq!(old_info.version, None);
        assert_eq!(
            Compatibility::for_protocol(old_info.protocol),
            Compatibility::Supported
        );
        assert_eq!(
            Compatibility::for_protocol(Some(1)),
            Compatibility::Supported
        );
        assert_eq!(
            Compatibility::for_protocol(Some(0)),
            Compatibility::ServerTooOld
        );
        assert_eq!(
            Compatibility::for_protocol(Some(PROTOCOL_VERSION + 1)),
            Compatibility::AppTooOld
        );
    }

    /// Joining or sharing while connecting, then leaving or stopping: once connected, the
    /// join or share must not run after all (#142).
    #[test]
    fn leaving_while_connecting_drops_the_waiting_join_and_shares() {
        let app = |id: &str| {
            Command::ShareApp(App {
                id: id.into(),
                name: "Music".into(),
            })
        };
        let mut queued = VecDeque::from([
            Command::JoinVoice([1; 32]),
            Command::Open([2; 32]),
            app("a"),
            app("b"),
        ]);
        cancel_queued(&mut queued, &Command::StopApp("a".into()));
        assert_eq!(queued.len(), 3);
        assert!(
            !queued
                .iter()
                .any(|command| matches!(command, Command::ShareApp(app) if app.id == "a"))
        );
        cancel_queued(&mut queued, &Command::LeaveVoice);
        // What reconnecting runs: only what has nothing to do with the call.
        assert_eq!(queued.len(), 1);
        assert!(matches!(queued[0], Command::Open(_)));
        cancel_queued(&mut queued, &Command::StopControl);
        assert_eq!(queued.len(), 1);
    }
}
