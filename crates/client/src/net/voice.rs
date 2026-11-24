//! The voice room: joining, sharing apps, screens, cameras and live files, and who is in it.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use livekit::{DisconnectReason, prelude::RemoteTrack};
use rusttalk_core::{
    auth::{decode, encode, voice_identity},
    camera::CameraOptions,
    client::{
        Event, KeyClient, MessageError, MessageRecord,
        voice::{
            ConnectionState, DataLiveFiles, LiveDownload, LiveFileEvent, NativeAudioSource,
            NativeVideoSource, PendingVoice, RemoteParticipant, RoomEvent, TrackSid, TrackSource,
            VoiceRoom, VoiceState, VoiceStatus,
        },
    },
    control::{Guest, Host},
    identity::fingerprint,
};
use url::Url;

use super::{
    Command, Ctx, LiveOffer, State, Update,
    control::{self, ControlView},
    phone,
    terminal::{self, Hosting},
};
use crate::{
    screen,
    server::{CallState, Id, Peer, Screen, Stats, TrackStats},
    share::{self, App, Capture},
    sounds::Snapshot,
};

type WatchOwners = HashMap<String, (String, TrackSource, String)>;

/// Retry transport failures, never removals, duplicate devices, or explicit closes.
fn recoverable_disconnect(reason: DisconnectReason) -> bool {
    matches!(
        reason,
        DisconnectReason::ServerShutdown
            | DisconnectReason::SignalClose
            | DisconnectReason::ConnectionTimeout
            | DisconnectReason::MediaFailure
            | DisconnectReason::StateMismatch
            | DisconnectReason::UnknownReason
            | DisconnectReason::JoinFailure
    )
}

pub(super) fn recovery_delay(attempt: u32) -> Option<Duration> {
    (attempt < 5).then(|| Duration::from_secs(1 << attempt))
}

/// Start fresh-auth recovery immediately on disconnect without overriding active retry backoff.
fn schedule_immediate_recovery(
    recovery: &mut Option<(std::time::Instant, u32)>,
    now: std::time::Instant,
) {
    match recovery {
        None | Some((_, 0)) => *recovery = Some((now, 0)),
        Some(_) => {}
    }
}

fn watch_on(
    watched: &mut HashSet<String>,
    owners: &mut WatchOwners,
    sid: &str,
    owner: Option<(String, TrackSource)>,
) {
    if let Some((owner, source)) = owner {
        owners.insert(sid.to_owned(), (owner, source, sid.to_owned()));
    }
    watched.insert(sid.to_owned());
}

/// Drop watch intent for `sid`, returning track SIDs that no remaining watch still needs.
fn watch_off(watched: &mut HashSet<String>, owners: &mut WatchOwners, sid: &str) -> Vec<String> {
    let removed: Vec<_> = owners
        .iter()
        .filter(|(requested, (_, _, current))| requested.as_str() == sid || current.as_str() == sid)
        .map(|(requested, (_, _, current))| (requested.clone(), current.clone()))
        .collect();
    for (requested, _) in &removed {
        owners.remove(requested);
    }
    let still_tracks =
        |target: &str, owners: &WatchOwners| owners.values().any(|(_, _, active)| active == target);
    let mut dropped = Vec::new();
    if !still_tracks(sid, owners) {
        watched.remove(sid);
        dropped.push(sid.to_owned());
    }
    for (_, current) in removed {
        if current != sid && !still_tracks(&current, owners) {
            watched.remove(&current);
            dropped.push(current);
        }
    }
    dropped
}

fn remap_watch(
    watched: &mut HashSet<String>,
    owners: &mut WatchOwners,
    participant: &str,
    source: TrackSource,
    sid: &str,
) -> Option<Vec<(String, String)>> {
    let mut matched = false;
    let mut replacements = Vec::new();
    for (owner, kind, current) in owners.values_mut() {
        if owner != participant || *kind != source {
            continue;
        }
        matched = true;
        if current != sid {
            let old = std::mem::replace(current, sid.to_owned());
            watched.remove(&old);
            watched.insert(sid.to_owned());
            if !replacements.iter().any(|(prev, _)| prev == &old) {
                replacements.push((old, sid.to_owned()));
            }
        }
    }
    matched.then_some(replacements)
}

fn unpublish_watch(watched: &mut HashSet<String>, owners: &mut WatchOwners, sid: &str) {
    let _ = watch_off(watched, owners, sid);
}

fn participant_left_watch(watched: &mut HashSet<String>, owners: &mut WatchOwners, identity: &str) {
    let removed: Vec<_> = owners
        .iter()
        .filter(|(_, (owner, _, _))| owner == identity)
        .map(|(requested, (_, _, current))| (requested.clone(), current.clone()))
        .collect();
    for (requested, current) in removed {
        owners.remove(&requested);
        watched.remove(&requested);
        watched.remove(&current);
    }
}

fn silence_lost_transport(ctx: &Ctx, url: &Url, live: &mut Live) {
    control::leave(live);
    live.guest = Default::default();
    live.control = Default::default();
    ctx.send(Update::Control(live.control.clone()));
    ctx.audio.stop_listening();
    ctx.audio.end_phone();
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if let Some(phone) = &live.phone {
        phone.stop.cancel();
    }
    ctx.send(Update::ExternalPhones(
        url.clone(),
        live.channel,
        Vec::new(),
    ));
}

/// The voice room this device is in, on this server.
pub(super) struct Live {
    pub(super) recovery: Option<(std::time::Instant, u32)>,
    pub(super) transport_closed: bool,
    camera_source: Option<(NativeVideoSource, CameraOptions)>,
    watch_owners: WatchOwners,
    pub(super) room: VoiceRoom,
    pub(super) channel: Id,
    /// The last acknowledged deafen state, so failures can be retried.
    deafened: Option<bool>,
    pub(super) phone_consents: Vec<rusttalk_core::client::voice::phone::Call>,
    pub(super) phone_announcement: Option<(
        Option<TrackSid>,
        Vec<rusttalk_core::client::voice::phone::Call>,
        Vec<rusttalk_core::client::voice::phone::Call>,
    )>,
    pub(super) phone_withdrawals: phone::Withdrawals,
    pub(super) phone_acknowledgements: Vec<rusttalk_core::client::voice::phone::Acknowledgement>,
    /// Apps this device shares, each with its own track.
    shares: Vec<Share>,
    /// The phone call bridged in, shown like a shared app.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    pub(super) phone: Option<phone::Phone>,
    /// App captures still starting; `app_started` publishes them.
    pending: Vec<Pending>,
    /// The sound the screen share should carry; started once the screen is live.
    stream_audio: Vec<App>,
    /// Decrypted names of subscribed remote app tracks.
    apps: HashMap<TrackSid, String>,
    /// Audio tracks from devices not known when they arrived, with their identity, to
    /// give them their person once known.
    nameless: HashMap<String, String>,
    /// A capture waiting for the person to pick (Linux) or for its first frame.
    pub(super) starting:
        Option<tokio::task::JoinHandle<Result<(screen::Capture, screen::Options)>>>,
    /// The published screen with its capture and quality, which a change keeps.
    pub(super) screen: Option<(TrackSid, screen::Capture, screen::Options)>,
    /// Picking another source for the screen; the old one keeps sharing meanwhile.
    pub(super) swap: screen::Swap<tokio::task::JoinHandle<Result<screen::Capture>>>,
    /// The published camera; the UI owns its capture.
    camera: Option<TrackSid>,
    /// Screen and camera tracks this device receives; others are unsubscribed.
    pub(super) watched: HashSet<String>,
    announced_watches: Option<Vec<TrackSid>>,
    /// Who asks for, or has, control of our screen.
    pub(super) host: Host,
    /// The screen we ask for, or have, control of.
    pub(super) guest: Guest,
    /// What the window last heard about both.
    pub(super) control: ControlView,
    /// Our shared terminal.
    pub(super) terminal: Option<Hosting>,
    /// The terminal we watch.
    pub(super) viewing: Option<rusttalk_core::terminal::Viewer>,
    /// When the call last rang here.
    rang: Option<std::time::Instant>,
}

/// A live file in the voice room or a chat, kept in `State::transfers` by transfer ID.
#[derive(Default)]
pub(super) struct Transfer {
    /// Offered in a text channel or DM, rather than in the call.
    pub(super) chat: bool,
    /// The text channel or DM, or the voice channel.
    pub(super) channel: Id,
    /// The offer from someone else; in the call only until accepted.
    pub(super) offer: Option<MessageRecord>,
    /// Our own offer.
    pub(super) mine: bool,
    /// Receiving it; aborting cancels it.
    pub(super) download: Option<tokio::task::JoinHandle<()>>,
}

impl Transfer {
    pub(super) fn chat(channel: Id) -> Self {
        Self {
            chat: true,
            channel,
            ..Self::default()
        }
    }
}

/// An app capture starting off the voice loop: macOS can wait on its permission prompt.
struct Pending {
    app: App,
    stream: bool,
    task: tokio::task::JoinHandle<Result<(Capture, NativeAudioSource)>>,
}

pub(super) struct Share {
    app: App,
    sid: TrackSid,
    /// Stops capturing when dropped.
    capture: Capture,
    source: NativeAudioSource,
    /// Part of the screen share: only its viewers hear it, and it stops with the share.
    stream: bool,
}

/// Commands that only need the voice room, which outlives the API connection.
pub(super) fn voice_only(command: &Command) -> bool {
    matches!(
        command,
        Command::LeaveVoice
            | Command::Ring
            | Command::SyncVoiceStatus
            | Command::StopApp(_)
            | Command::StopScreen
            | Command::StreamAudio(_)
            | Command::ChangeScreen(..)
            | Command::Watch(..)
            | Command::Listen(..)
            | Command::PhoneConsent(..)
            | Command::StopCamera
            | Command::CancelFile(_)
    ) || control::is_control(command)
        || terminal::is_terminal(command)
        || phone::is_phone(command)
}

/// Run a `voice_only` command. Returns the call's channel when its roster changed.
pub(super) async fn voice_command(
    ctx: &Ctx,
    url: &Url,
    state: &mut State,
    command: Command,
) -> Result<Option<Id>> {
    if let Command::LeaveVoice = command {
        return Ok(close(ctx, state));
    }
    if let Command::CancelFile(id) = command {
        cancel_file(state, &id);
        return Ok(None);
    }
    if let Command::SyncVoiceStatus = command {
        sync_voice_status(ctx, state).await;
        return Ok(None);
    }
    let Some(live) = &mut state.voice else {
        return Ok(None);
    };
    if control::is_control(&command) {
        control::command(ctx, live, command).await;
        return Ok(None);
    }
    if terminal::is_terminal(&command) {
        let changed = terminal::command(ctx, live, command).await;
        return Ok(changed.then_some(live.channel));
    }
    if let Command::Ring = command {
        // The request went out as a DM either way; without the right to send data here
        // only the two of them hear it.
        if let Err(error) = live.room.send_ring().await {
            tracing::info!(error = %format_args!("{error:#}"), "ringing the call failed");
        }
        return Ok(None);
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if let Command::PhoneCall(dial) = command {
        let changed = phone::call(ctx, live, *dial).await;
        return Ok(changed.then_some(live.channel));
    }
    let changed = match command {
        Command::PhoneConsent(call, on) => {
            if !on {
                ctx.audio.pause_phone_withdrawal(true);
                live.phone_withdrawals.start(call.clone());
                ctx.audio.accept_phone_withdrawal_request();
            }
            live.phone_consents.retain(|old| old != &call);
            if on && live.room.external_phone_calls().contains(&call) {
                live.phone_consents.push(call);
            }
            phone::sync(ctx, url, live).await;
            false
        }
        Command::StopApp(id) => {
            live.pending
                .retain(|start| start.stream || start.app.id != id);
            let Some(at) =
                (live.shares.iter()).position(|share| !share.stream && share.app.id == id)
            else {
                return Ok(None);
            };
            let share = live.shares.remove(at);
            tracing::info!("stopped sharing app audio");
            if let Err(error) = live.room.unpublish_audio(&share.sid).await {
                tracing::warn!(error = %format_args!("{error:#}"), "stopping app audio failed");
            }
            true
        }
        Command::StopScreen => {
            let sid = live.screen.as_ref().map(|(sid, ..)| sid.clone());
            let changed = stop_screen(ctx, live).await;
            if let Some(sid) = sid {
                ctx.send(Update::CallVideoStopped(url.clone(), live.channel, sid));
            }
            changed
        }
        Command::StreamAudio(apps) => set_stream_audio(ctx, live, apps).await,
        Command::ChangeScreen(target, remote_control) => {
            // Control was granted for what is shared now, not what comes next.
            control::stop_hosting(ctx, live).await;
            change_screen(ctx, live, target, remote_control);
            false
        }
        Command::Listen(sid, on) => {
            live.room.set_listening(
                &TrackSid::try_from(sid.clone()).map_err(anyhow::Error::msg)?,
                on,
            )?;
            ctx.audio.set_listening(&sid, on);
            true
        }
        Command::Watch(sid, on) => {
            watch(ctx, live, sid, on)?;
            sync_video_watches(live).await;
            true
        }
        Command::StopCamera => {
            let sid = live.camera.clone();
            stop_camera(live).await;
            if let Some(sid) = sid {
                ctx.send(Update::CallVideoStopped(url.clone(), live.channel, sid));
            }
            true
        }
        _ => false,
    };
    Ok(changed.then_some(live.channel))
}

/// Stop the screen share, closing a system picker that is still open. True if a share stopped.
pub(super) async fn stop_screen(ctx: &Ctx, live: &mut Live) -> bool {
    // Remote control ends with the share; release keys while it can still inject.
    control::stop_hosting(ctx, live).await;
    if let Some(starting) = live.starting.take() {
        starting.abort();
    }
    if let Some(picker) = live.swap.abort() {
        picker.abort();
    }
    live.stream_audio.clear();
    let (sound, kept) = std::mem::take(&mut live.shares)
        .into_iter()
        .partition(|share| share.stream);
    live.shares = kept;
    unpublish_shares(live, sound).await;
    let Some((sid, ..)) = live.screen.take() else {
        return false;
    };
    tracing::info!("stopped sharing screen");
    if let Err(error) = live.room.unpublish_video(&sid).await {
        tracing::warn!(error = %format_args!("{error:#}"), "stopping screen failed");
    }
    true
}

async fn unpublish_shares(live: &Live, shares: Vec<Share>) {
    for share in shares {
        tracing::info!(stream = share.stream, "stopped sharing app audio");
        if let Err(error) = live.room.unpublish_audio(&share.sid).await {
            tracing::warn!(error = %format_args!("{error:#}"), "stopping app audio failed");
        }
    }
}

/// Make the screen share carry `apps`' sound: stop the others and start the new ones.
/// Before the screen is live this only notes them. True if a track changed.
async fn set_stream_audio(ctx: &Ctx, live: &mut Live, apps: Vec<App>) -> bool {
    if !apps.is_empty() && !live.room.can_share_audio() {
        live.stream_audio.clear();
        ctx.send(Update::StreamAudio(Vec::new()));
        return false;
    }
    live.stream_audio = apps;
    if live.screen.is_none() {
        return false;
    }
    let wanted = |id: &str| live.stream_audio.iter().any(|app| app.id == id);
    let (gone, kept): (Vec<_>, _) = std::mem::take(&mut live.shares)
        .into_iter()
        .partition(|share| share.stream && !wanted(&share.app.id));
    live.shares = kept;
    live.pending
        .retain(|start| !start.stream || wanted(&start.app.id));
    let changed = !gone.is_empty();
    unpublish_shares(live, gone).await;
    for app in live.stream_audio.clone() {
        let started = |id: &str| id == app.id;
        if live.shares.iter().any(|s| s.stream && started(&s.app.id))
            || live.pending.iter().any(|s| s.stream && started(&s.app.id))
        {
            continue;
        }
        start_share(live, app, true);
    }
    ctx.send(Update::StreamAudio(live.stream_audio.clone()));
    changed
}

/// Start capturing an app (or all system audio) without holding up the call; starting
/// waits for its first audio, or for the macOS permission prompt.
fn start_share(live: &mut Live, app: App, stream: bool) {
    let target = app.clone();
    let task = tokio::task::spawn_blocking(move || share::capture(&target));
    live.pending.push(Pending { app, stream, task });
}

/// An app capture finished starting: publish it as its own track if still wanted.
pub(super) async fn app_started(
    ctx: &Ctx,
    url: &Url,
    client: Option<&KeyClient<'_>>,
    state: &mut State,
    app: App,
    stream: bool,
    started: Result<(Capture, NativeAudioSource)>,
) {
    let Some(live) = &mut state.voice else {
        return;
    };
    // Live, sound goes with the stream; stream sound only while the screen is live.
    let wanted = match stream {
        true => live.screen.is_some() && live.stream_audio.iter().any(|w| w.id == app.id),
        false => live.screen.is_none(),
    };
    if !wanted {
        if !stream {
            ctx.send(Update::AppStopped(app.id));
        }
        return;
    }
    let published = match started {
        Ok((capture, source)) => {
            publish_share(&live.room, app.clone(), stream, capture, source).await
        }
        Err(error) => Err(error),
    };
    match published {
        Ok(share) => {
            tracing::info!(stream, "sharing app audio");
            live.shares.push(share);
            let channel = live.channel;
            roster(ctx, url, client, state, channel);
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "sharing app audio failed");
            ctx.send(Update::Failed(share::failed(&error, &app)));
            if stream {
                live.stream_audio.retain(|wanted| wanted.id != app.id);
                ctx.send(Update::StreamAudio(live.stream_audio.clone()));
            } else {
                ctx.send(Update::AppStopped(app.id));
            }
        }
    }
}

/// Publish a started app capture as its own track.
async fn publish_share(
    room: &VoiceRoom,
    app: App,
    stream: bool,
    capture: Capture,
    source: NativeAudioSource,
) -> Result<Share> {
    let publication = if stream {
        room.publish_stream_audio(&app.name, source.clone()).await?
    } else {
        room.publish_app_audio(&app.name, source.clone()).await?
    };
    Ok(Share {
        app,
        sid: publication.sid(),
        capture,
        source,
        stream,
    })
}

/// Stop offering our file, or stop receiving one; dropping a download removes its partial file.
fn cancel_file(state: &mut State, id: &Id) {
    let Some(transfer) = state.transfers.get_mut(id) else {
        return;
    };
    if std::mem::take(&mut transfer.mine) {
        tracing::info!(chat = transfer.chat, "stopped offering a live file");
        if transfer.chat {
            if let Some(files) = state.chat_files.get_mut(&transfer.channel) {
                files.cancel_live_offer(id);
            }
        } else if let Some(live) = &mut state.voice {
            live.room.cancel_live_offer(id);
        }
    } else if let Some(download) = transfer.download.take() {
        tracing::info!(chat = transfer.chat, "cancelled a live file download");
        download.abort();
    }
}

pub(super) async fn stop_camera(live: &mut Live) {
    live.camera_source = None;
    if let Some(sid) = live.camera.take() {
        tracing::info!("stopped sharing camera");
        if let Err(error) = live.room.unpublish_video(&sid).await {
            tracing::warn!(error = %format_args!("{error:#}"), "stopping camera failed");
        }
    }
}

/// Start or stop receiving a screen or camera track.
pub(super) fn watch(ctx: &Ctx, live: &mut Live, sid: String, on: bool) -> Result<()> {
    if on {
        let owner = live
            .room
            .remote_video_tracks()
            .into_iter()
            .find(|v| v.sid.as_str() == sid)
            .map(|v| (v.participant.to_string(), v.source));
        watch_on(&mut live.watched, &mut live.watch_owners, &sid, owner);
    } else {
        let dropped = watch_off(&mut live.watched, &mut live.watch_owners, &sid);
        for track in &dropped {
            ctx.video.remove_track(track);
            if track != &sid
                && let Ok(track_sid) = TrackSid::try_from(track.clone())
            {
                let _ = live.room.set_subscribed(&track_sid, false);
            }
        }
        if !dropped.iter().any(|track| track == &sid) {
            return Ok(());
        }
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
    if !on {
        for (identity, audio, _) in live.room.shared_audio() {
            if identity == video.participant && !live.room.is_listening(&audio) {
                ctx.audio.set_listening(audio.as_str(), false);
            }
        }
    }
    // Already subscribed: no new event will come.
    if let (true, Some(track)) = (on, video.track) {
        ctx.video.add_track(sid, track.rtc_track());
    }
    Ok(())
}

/// Use the subscriptions that actually supply video to the call stage.
pub(super) async fn sync_video_watches(live: &mut Live) {
    if live.room.connection_state() != ConnectionState::Connected {
        return;
    }
    let mut tracks: Vec<_> = live
        .room
        .remote_video_tracks()
        .into_iter()
        .filter(|video| video.track.is_some() && live.watched.contains(video.sid.as_str()))
        .map(|video| video.sid)
        .collect();
    tracks.sort_unstable();
    if live.announced_watches.as_ref() == Some(&tracks) {
        return;
    }
    match live.room.set_video_watches(&tracks).await {
        Ok(()) => live.announced_watches = Some(tracks),
        Err(error) => tracing::warn!(%error, "announcing video subscriptions failed"),
    }
}

/// Publish a capture that started, or report why it didn't.
pub(super) async fn share_screen(
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
            .map(|publication| (publication.sid(), capture, options)),
        Err(error) => Err(error),
    };
    match published {
        Ok(screen) => {
            tracing::info!("sharing screen");
            live.screen = Some(screen);
            ctx.send(Update::Sharing(true));
            // Live, sound is part of the stream: apps shared on their own stop (the
            // window offered them for the stream instead).
            let (apps, sound): (Vec<_>, _) = std::mem::take(&mut live.shares)
                .into_iter()
                .partition(|share| !share.stream);
            live.shares = sound;
            for share in &apps {
                ctx.send(Update::AppStopped(share.app.id.clone()));
            }
            unpublish_shares(live, apps).await;
            let wanted = live.stream_audio.clone();
            set_stream_audio(ctx, live, wanted).await;
            let channel = live.channel;
            send_room(ctx, url, client, state, channel);
        }
        Err(error) if error.is::<screen::Cancelled>() => {
            tracing::info!("screen picker cancelled");
            ctx.send(Update::Sharing(false));
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "sharing screen failed");
            ctx.send(Update::Sharing(false));
            ctx.send(Update::Failed(t!("voice.screen_failed").into()));
        }
    }
}

/// A voice join still connecting, off the server loop so chat and leaving keep working.
/// Dropping it cancels the join; the task then leaves the room as soon as it connects,
/// since a LiveKit connect dropped halfway could leave us listed in the room.
pub(super) struct Joining {
    pub(super) channel: Id,
    room: tokio::sync::oneshot::Receiver<Result<VoiceRoom>>,
    /// Call commands sent while connecting, run once the join ends, as before.
    pub(super) deferred: Vec<Command>,
}

impl Drop for Joining {
    /// Dropped between the join finishing and `serve` taking its room: leave that room too.
    fn drop(&mut self) {
        if let Ok(Ok(room)) = self.room.try_recv()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(leave_room(room));
        }
    }
}

/// Connect and publish the microphone when allowed to talk.
fn spawn_join(ctx: &Ctx, pending: PendingVoice, channel: Id) -> Joining {
    let (done, room) = tokio::sync::oneshot::channel();
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let joined = async {
            let room = pending.connect().await?;
            // Cancelled meanwhile: never publish the microphone.
            if room.can_publish() && !done.is_closed() {
                if let Err(error) = room.publish_audio(ctx.audio.source()).await {
                    leave_room(room).await;
                    return Err(error);
                }
                room.set_microphone_muted(ctx.audio.muted() || ctx.audio.deafened());
            }
            Ok(room)
        }
        .await;
        if let Err(Ok(room)) = done.send(joined) {
            leave_room(room).await;
        }
    });
    Joining {
        channel,
        room,
        deferred: Vec::new(),
    }
}

/// The result of the join in `joining`, once it is done.
pub(super) async fn next_join(joining: &mut Option<Joining>) -> Result<VoiceRoom> {
    match joining {
        Some(joining) => (&mut joining.room)
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("voice join task ended"))),
        None => std::future::pending().await,
    }
}

/// Commands that need the call, so they wait while it is still connecting.
pub(super) fn waits_for_call(command: &Command) -> bool {
    !matches!(command, Command::LeaveVoice) && voice_only(command)
        || matches!(
            command,
            Command::ShareApp(_) | Command::ShareScreen(..) | Command::ShareCamera(..)
        )
}

/// Replace only the transport, retaining the user's current capture choices.
pub(super) async fn restore_media(ctx: &Ctx, url: &Url, live: &mut Live) -> Result<()> {
    live.transport_closed = false;
    live.recovery = None;
    live.deafened = None;
    live.announced_watches = None;
    live.apps.clear();
    live.nameless.clear();
    if live.room.can_publish() {
        live.room.publish_audio(ctx.audio.source()).await?;
        live.room
            .set_microphone_muted(ctx.audio.muted() || ctx.audio.deafened());
    }
    if !live.room.can_share_screen() {
        stop_screen(ctx, live).await;
        ctx.send(Update::Sharing(false));
    } else if let Some((sid, capture, options)) = &mut live.screen {
        if !capture.is_finished() {
            *sid = live
                .room
                .publish_screen(&capture.name, *options, capture.source.clone())
                .await?
                .sid();
        } else {
            stop_screen(ctx, live).await;
            ctx.send(Update::Sharing(false));
        }
    }
    if let Some((source, options)) = &live.camera_source {
        if live.room.can_use_camera() {
            live.camera = Some(
                live.room
                    .publish_camera(*options, source.clone())
                    .await?
                    .sid(),
            );
        } else {
            stop_camera(live).await;
            ctx.send(Update::CameraStopped);
        }
    }
    if live.room.can_share_audio() {
        for share in &mut live.shares {
            if share.capture.is_finished() {
                continue;
            }
            share.sid = if share.stream {
                live.room
                    .publish_stream_audio(&share.app.name, share.source.clone())
                    .await?
                    .sid()
            } else {
                live.room
                    .publish_app_audio(&share.app.name, share.source.clone())
                    .await?
                    .sid()
            };
        }
    } else {
        live.stop_audio_captures();
    }
    restore_watches(ctx, url, live);
    Ok(())
}

fn restore_watches(ctx: &Ctx, url: &Url, live: &mut Live) {
    for video in live.room.remote_video_tracks() {
        let sid = video.sid.to_string();
        let Some(replacements) = remap_watch(
            &mut live.watched,
            &mut live.watch_owners,
            video.participant.as_str(),
            video.source,
            &sid,
        ) else {
            continue;
        };
        for (old, new) in replacements {
            ctx.video.remove_track(&old);
            ctx.send(Update::VideoReplaced(url.clone(), live.channel, old, new));
        }
        if video.track.is_none()
            && let Err(error) = live.room.set_subscribed(&video.sid, true)
        {
            tracing::warn!(%error, "restoring video subscription failed");
        }
    }
}

/// Apply the channel's audio bitrate to the call, republishing our audio tracks if it changed.
pub(super) async fn set_audio_kbps(state: &mut State, kbps: u32) {
    let Some(live) = &mut state.voice else {
        return;
    };
    match live.room.set_audio_kbps(kbps).await {
        Ok(replaced) => {
            for (old, new) in replaced {
                if let Some(share) = live.shares.iter_mut().find(|share| share.sid == old) {
                    share.sid = new;
                }
            }
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "applying audio bitrate failed")
        }
    }
}

pub(super) async fn sync_voice_status(ctx: &Ctx, state: &mut State) {
    let Some(live) = &mut state.voice else {
        return;
    };
    let deafened = ctx.audio.deafened();
    live.room
        .set_microphone_muted(ctx.audio.muted() || deafened);
    if live.deafened != Some(deafened) && live.room.connection_state() == ConnectionState::Connected
    {
        match live.room.set_deafened(deafened).await {
            Ok(()) => live.deafened = Some(deafened),
            Err(error) => tracing::warn!(%error, "sharing deafen state failed"),
        }
    }
}

pub(super) fn leave_voice(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, state: &mut State) {
    if let Some(channel) = close(ctx, state) {
        send_room(ctx, url, client, state, channel);
    }
}

/// Leave a call: stop showing its videos, close a system picker that is still open,
/// and close the room. Returns the call's channel.
pub(super) fn close(ctx: &Ctx, state: &mut State) -> Option<Id> {
    // Dropping a join still connecting cancels it.
    let joining = state.joining.take().map(|joining| joining.channel);
    let Some(mut live) = state.voice.take() else {
        return joining;
    };
    control::leave(&mut live);
    left_room(state, &live.channel, ctx.identity.certificate.person_id());
    // The call's live files go with it.
    state.transfers.retain(|_, transfer| transfer.chat);
    for sid in &live.watched {
        ctx.video.remove_track(sid);
    }
    if let Some(starting) = &live.starting {
        starting.abort();
    }
    if let screen::Swap::Picking(picker) = &live.swap {
        picker.abort();
    }
    // Closing waits for the media server; nothing here needs to wait for it.
    tokio::spawn(leave_room(live.room));
    Some(live.channel)
}

/// Drop ourselves from the server's last roster of a call we just left. That roster
/// was fetched while we were in the call, and the server's next one only comes after
/// the media server reports the leave: until then the sidebar would list us in both the
/// old and the new channel. Our other devices still in it come back with that refetch.
fn left_room(state: &mut State, channel: &Id, me: &Id) {
    if let Some(roster) = state.rooms.get_mut(channel) {
        roster.people.retain(|person| person != me);
    }
}

pub(super) async fn leave_room(room: VoiceRoom) {
    tracing::info!("left voice");
    if let Err(error) = room.leave().await {
        tracing::warn!(error = %format_args!("{error:#}"), "leaving voice failed");
    }
}

/// Install a newer media key after key work for the voice channel.
pub(super) fn sync_voice_key(ctx: &Ctx, client: &KeyClient<'_>, state: &mut State) {
    if let Some(live) = &mut state.voice
        && let Err(error) = live.room.sync_key(client)
    {
        tracing::warn!(error = %format_args!("{error:#}"), "voice key rotation pending");
        if error.downcast_ref::<MessageError>() == Some(&MessageError::ServerUpdateRequired) {
            close(ctx, state);
        }
    }
}

// Lives only between a select and its handler.
#[allow(clippy::large_enum_variant)]
pub(super) enum Next {
    Event(Option<RoomEvent>),
    /// A screen capture finished starting; clear `starting` before polling again.
    Capture(Result<(screen::Capture, screen::Options)>),
    /// The picker for another screen source finished.
    Swap(Result<screen::Capture>),
    /// An app capture finished starting; it's no longer in `pending`.
    App(App, bool, Result<(Capture, NativeAudioSource)>),
    /// Time to send our terminal's output.
    Flush,
}

pub(super) async fn next_voice(voice: &mut Option<Live>) -> Next {
    let Some(live) = voice else {
        return std::future::pending().await;
    };
    if live.transport_closed {
        return std::future::pending().await;
    }
    let starting = async {
        match live.starting.as_mut() {
            Some(starting) => starting.await,
            None => std::future::pending().await,
        }
    };
    let swapping = async {
        match &mut live.swap {
            screen::Swap::Picking(picker) => picker.await,
            screen::Swap::Idle => std::future::pending().await,
        }
    };
    let app = async {
        if live.pending.is_empty() {
            return std::future::pending().await;
        }
        let tasks = live.pending.iter_mut().map(|start| &mut start.task);
        let (started, at, _) = futures_util::future::select_all(tasks).await;
        let start = live.pending.remove(at);
        let started = started.map_err(anyhow::Error::from).and_then(|s| s);
        Next::App(start.app, start.stream, started)
    };
    let flush = async {
        match live.terminal.as_mut() {
            Some(hosting) => hosting.tick.tick().await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        event = live.room.next_event() => Next::Event(event),
        _ = flush => Next::Flush,
        capture = starting => Next::Capture(capture.map_err(anyhow::Error::from).and_then(|c| c)),
        capture = swapping => Next::Swap(capture.map_err(anyhow::Error::from).and_then(|c| c)),
        next = app => next,
    }
}

/// A sealed remote control message from someone in the call. Without `client` (the API
/// connection is down) the sender isn't named, so they can't ask for control; ending it works.
pub(super) async fn control_received(
    ctx: &Ctx,
    client: Option<&KeyClient<'_>>,
    state: &mut State,
    from: &RemoteParticipant,
    wire: &[u8],
) {
    let from = from.identity();
    let person = client.and_then(|client| {
        identities(client, &state.server_id)
            .get(from.as_str())
            .copied()
    });
    if let Some(live) = &mut state.voice {
        control::received(ctx, live, &from, person, wire).await;
    }
}

/// A sealed ring from someone in the call. One every few seconds at most, whoever sends
/// it, and only from a known person, so blocking and muting them silences it.
pub(super) fn ring_received(
    ctx: &Ctx,
    url: &Url,
    client: Option<&KeyClient<'_>>,
    state: &mut State,
    from: &RemoteParticipant,
    wire: &[u8],
) {
    const APART: std::time::Duration = std::time::Duration::from_secs(3);
    let from = from.identity();
    let Some(live) = &mut state.voice else {
        return;
    };
    if !live.room.open_ring(&from, wire) {
        tracing::warn!("dropped a ring that did not authenticate");
        return;
    }
    if live.rang.is_some_and(|at| at.elapsed() < APART) {
        return;
    }
    let person = match client {
        Some(client) => identities(client, &state.server_id)
            .get(from.as_str())
            .copied(),
        None => state.identities.get(from.as_str()).copied(),
    };
    let Some(person) = person else {
        tracing::info!("dropped a ring from an unknown device");
        return;
    };
    live.rang = Some(std::time::Instant::now());
    ctx.send(Update::CallRang(url.clone(), live.channel, person));
}

/// A sealed terminal message from someone in the call; `client` as for `control_received`.
pub(super) async fn terminal_received(
    ctx: &Ctx,
    client: Option<&KeyClient<'_>>,
    state: &mut State,
    from: &RemoteParticipant,
    wire: &[u8],
) {
    let from = from.identity();
    let person = client.and_then(|client| {
        identities(client, &state.server_id)
            .get(from.as_str())
            .copied()
    });
    if let Some(live) = &mut state.voice {
        terminal::received(ctx, live, &from, person, wire).await;
    }
}

/// Give audio from devices that weren't known when it arrived its person, now that the
/// API names them (after reconnecting, or once their device is synced).
pub(super) fn name_audio(ctx: &Ctx, client: &KeyClient<'_>, state: &mut State) {
    let Some(live) = &mut state.voice else {
        return;
    };
    if live.nameless.is_empty() {
        return;
    }
    let ids = identities(client, &state.server_id);
    live.nameless
        .retain(|sid, identity| match ids.get(identity.as_str()) {
            Some(person) => {
                ctx.audio.set_person(sid, *person);
                false
            }
            None => true,
        });
}

/// LiveKit identity of every known device, to find the person behind a voice participant.
pub(super) fn identities(client: &KeyClient<'_>, server: &Id) -> HashMap<String, Id> {
    client
        .devices()
        .map(|device| {
            let id = voice_identity(server, device.signing_key().as_bytes());
            (id, *device.person_id())
        })
        .collect()
}

pub(super) async fn room_changed(
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
pub(super) fn send_room(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, state: &State, channel: Id) {
    send_roster(
        ctx,
        url,
        &identities(client, &state.server_id),
        state,
        channel,
    );
}

fn screen_viewers(watches: &HashSet<(Id, String)>, sharer: &Id, sid: &str) -> usize {
    watches
        .iter()
        .filter(|(person, track)| person != sharer && track == sid)
        .count()
}

/// Send the people in `channel` to the window, `ids` naming the call's participants.
fn send_roster(ctx: &Ctx, url: &Url, ids: &HashMap<String, Id>, state: &State, channel: Id) {
    let me = *ctx.identity.certificate.person_id();
    let roster = state.rooms.get(&channel).cloned().unwrap_or_default();
    let mut people = roster.people;
    let mut peers = Vec::new();
    let mut call_sounds = None;
    // Devices in our call that can't be named (yet), shown as unknown like the server's.
    let mut nameless = 0;
    match state.voice.as_ref().filter(|live| live.channel == channel) {
        None => peers.extend(people.into_iter().map(|person| Peer {
            person,
            connecting: false,
            unknown: false,
            status: VoiceStatus::default(),
            apps: Vec::new(),
            screens: Vec::new(),
            camera: None,
            terminal: None,
        })),
        Some(live) => {
            let mut live_people: HashMap<Id, bool> = HashMap::new();
            let mut statuses: HashMap<Id, VoiceStatus> = HashMap::new();
            let mut apps: HashMap<Id, Vec<(TrackSid, String)>> = HashMap::new();
            let mut screens: HashMap<Id, Vec<Screen>> = HashMap::new();
            let mut cameras: HashMap<Id, String> = HashMap::new();
            let mut terminals: HashMap<Id, String> = HashMap::new();
            let mut watches: HashSet<(Id, String)> = HashSet::new();
            let mut sounds = Snapshot {
                camera: live.camera.clone(),
                screen: live.screen.as_ref().map(|(sid, ..)| sid.clone()),
                ..Snapshot::default()
            };
            for (participant, voice) in live.room.participants() {
                let person = ids.get(participant.identity().as_str());
                if person.is_none() && voice != VoiceState::Disconnected {
                    nameless += 1;
                }
                if let Some(person) = person {
                    *live_people.entry(*person).or_default() |= voice == VoiceState::Connected;
                    // One per person: a second device's terminal shows after the first stops.
                    if voice != VoiceState::Disconnected
                        && live.room.terminal_share(&participant).is_some()
                    {
                        terminals
                            .entry(*person)
                            .or_insert_with(|| participant.identity().to_string());
                    }
                    if *person != me && voice != VoiceState::Disconnected {
                        sounds.people.insert(*person);
                    }
                    if voice == VoiceState::Connected {
                        watches.extend(
                            live.room
                                .video_watches(&participant)
                                .into_iter()
                                .map(|sid| (*person, sid.to_string())),
                        );
                        if *person != me && (sounds.camera.is_some() || sounds.screen.is_some()) {
                            sounds.viewers.extend(
                                live.room
                                    .video_watches(&participant)
                                    .into_iter()
                                    .filter(|sid| {
                                        sounds.camera.as_ref() == Some(sid)
                                            || sounds.screen.as_ref() == Some(sid)
                                    })
                                    .map(|sid| (*person, sid)),
                            );
                        }
                        let status = VoiceStatus::from_participant(&participant);
                        // A person gets a badge only when every connected device agrees.
                        statuses
                            .entry(*person)
                            .and_modify(|current| {
                                current.muted &= status.muted;
                                current.deafened &= status.deafened;
                            })
                            .or_insert(status);
                    }
                }
            }
            for (identity, sid, name) in live.room.shared_audio() {
                if let Some(person) = ids.get(identity.as_str()) {
                    apps.entry(*person).or_default().push((sid, name));
                }
            }
            for names in apps.values_mut() {
                names.sort_by(|a, b| a.1.cmp(&b.1));
            }
            let mine = live
                .shares
                .iter()
                .map(|share| (share.sid.clone(), share.app.name.clone()));
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let mine = mine
                .chain((live.phone.iter()).map(|phone| (phone.sid.clone(), phone.label.clone())));
            apps.insert(me, mine.collect());
            for video in live.room.remote_video_tracks() {
                let Some(person) = ids.get(video.participant.as_str()) else {
                    continue;
                };
                match video.source {
                    TrackSource::Screenshare => screens.entry(*person).or_default().push(Screen {
                        sid: video.sid.to_string(),
                        name: video.name,
                        viewers: 0,
                        local: false,
                    }),
                    // One per person: a second device's camera shows after the first stops.
                    TrackSource::Camera => {
                        cameras.entry(*person).or_insert(video.sid.to_string());
                    }
                    _ => {}
                }
            }
            if let Some(sid) = &live.camera {
                cameras.insert(me, sid.to_string());
            }
            if live.terminal.is_some() {
                terminals.insert(me, live.room.identity().to_string());
            }
            if let Some((sid, ..)) = &live.screen {
                screens.entry(me).or_default().push(Screen {
                    sid: sid.to_string(),
                    name: t!("call.your_screen").into(),
                    viewers: 0,
                    local: true,
                });
            }
            // Include this device only after an actual video subscription was announced.
            if live.room.connection_state() == ConnectionState::Connected {
                watches.extend(
                    live.announced_watches
                        .iter()
                        .flatten()
                        .map(|sid| (me, sid.to_string())),
                );
            }
            for (sharer, streams) in &mut screens {
                streams.sort_by(|a, b| a.sid.cmp(&b.sid));
                for stream in streams {
                    stream.viewers = screen_viewers(&watches, sharer, &stream.sid);
                }
            }
            people.extend(live_people.keys().copied());
            people.push(me);
            people.sort();
            people.dedup();
            let connected = live.room.connection_state() == ConnectionState::Connected;
            call_sounds = connected.then_some(sounds);
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
                    status: statuses.remove(&person).unwrap_or_default(),
                    apps: apps.remove(&person).unwrap_or_default(),
                    screens: screens.remove(&person).unwrap_or_default(),
                    camera: cameras.remove(&person),
                    terminal: terminals.remove(&person),
                });
            }
        }
    }
    // The server's unknown people are likely the same devices: count them once.
    peers.extend((0..roster.unknown.max(nameless)).map(|_| Peer {
        person: [0; 32],
        connecting: false,
        unknown: true,
        status: VoiceStatus::default(),
        apps: Vec::new(),
        screens: Vec::new(),
        camera: None,
        terminal: None,
    }));
    ctx.send(Update::Room(url.clone(), channel, peers, call_sounds));
}

/// Send the people in `channel` to the window. Without `client` (the API connection is
/// down) the call's participants are named as last known; reconnecting reloads them.
pub(super) fn roster(
    ctx: &Ctx,
    url: &Url,
    client: Option<&KeyClient<'_>>,
    state: &State,
    channel: Id,
) {
    match client {
        Some(client) => send_room(ctx, url, client, state, channel),
        None => send_roster(ctx, url, &state.identities, state, channel),
    }
}

/// Handle a voice room event; `client` as for `roster`.
pub(super) async fn on_voice(
    ctx: &Ctx,
    url: &Url,
    client: Option<&KeyClient<'_>>,
    state: &mut State,
    event: Option<RoomEvent>,
) {
    let Some(channel) = state.voice.as_ref().map(|live| live.channel) else {
        return;
    };
    let end = |state: &mut State, text: &str| {
        close(ctx, state);
        ctx.send(Update::Call(url.clone(), channel, None));
        ctx.send(Update::Failed(text.into()));
    };
    let Some(event) = event else {
        tracing::warn!("voice room closed");
        if let Some(live) = &mut state.voice {
            silence_lost_transport(ctx, url, live);
            live.transport_closed = true;
            schedule_immediate_recovery(&mut live.recovery, std::time::Instant::now());
        }
        ctx.send(Update::Call(
            url.clone(),
            channel,
            Some(CallState::Reconnecting),
        ));
        roster(ctx, url, client, state, channel);
        return;
    };
    if let Some(live) = &state.voice {
        for (_, sid, _) in live.room.shared_audio() {
            if !live.room.is_listening(&sid) {
                ctx.audio.set_listening(sid.as_str(), false);
            }
        }
        let allowed = live.room.remote_audio_tracks();
        for sid in live.apps.keys() {
            if !allowed.iter().any(|(_, active, _)| active == sid) {
                ctx.audio.remove_track(sid.as_str());
            }
        }
    }
    if let Some(live) = &mut state.voice {
        restore_watches(ctx, url, live);
    }
    match event {
        RoomEvent::TrackSubscribed {
            track: RemoteTrack::Audio(track),
            participant,
            ..
        } => {
            let ids = client.map(|client| identities(client, &state.server_id));
            let ids = ids.as_ref().unwrap_or(&state.identities);
            let person = ids.get(participant.identity().as_str()).copied();
            if person.is_none() {
                tracing::warn!("voice track from a device this client doesn't know yet");
            }
            let sid = track.sid();
            let mut app = None;
            if let Some(live) = &mut state.voice {
                if person.is_none() {
                    let identity = participant.identity().to_string();
                    live.nameless.insert(sid.to_string(), identity);
                }
                if let Some(name) = live.room.app_audio_name(&track) {
                    app = Some(name.clone());
                    live.apps.insert(sid.clone(), name);
                }
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
            match state.voice.as_mut() {
                Some(live) if live.watched.contains(&sid) => {
                    ctx.video.add_track(sid, track.rtc_track());
                    sync_video_watches(live).await;
                }
                // Watching stopped while subscribing.
                Some(live) => {
                    if let Err(error) = live.room.set_subscribed(&publication.sid(), false) {
                        tracing::warn!(error = %format_args!("{error:#}"), "unsubscribing a video failed");
                    }
                }
                None => {}
            }
        }
        RoomEvent::TrackUnsubscribed { track, .. } => {
            if let Some(live) = &mut state.voice {
                live.apps.remove(&track.sid());
                live.nameless.remove(track.sid().as_str());
                if matches!(track, RemoteTrack::Video(_)) {
                    sync_video_watches(live).await;
                }
            }
            ctx.audio.remove_track(track.sid().as_str());
            ctx.video.remove_track(track.sid().as_str());
        }
        RoomEvent::Reconnecting => {
            if let Some(live) = &mut state.voice {
                control::leave(live);
                live.guest = Default::default();
                live.control = Default::default();
                ctx.send(Update::Control(live.control.clone()));
                live.recovery
                    .get_or_insert((std::time::Instant::now() + Duration::from_secs(10), 0));
                for call in live.phone_consents.drain(..) {
                    ctx.audio.pause_phone_withdrawal(true);
                    live.phone_withdrawals.start(call);
                }
                live.phone_announcement = None;
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                if let Some(phone) = &live.phone {
                    // Keep the publication until bounded SIP cleanup finishes, so the
                    // usual completion path can unpublish it after reconnecting.
                    phone.stop.cancel();
                }
                ctx.audio.end_phone();
            }
            ctx.audio.stop_listening();
            tracing::info!("voice reconnecting");
            ctx.send(Update::Call(
                url.clone(),
                channel,
                Some(CallState::Reconnecting),
            ));
        }
        RoomEvent::Reconnected => {
            ctx.audio.stop_listening();
            tracing::info!("voice transport reconnected; renewing authorization");
            if let Some(live) = &mut state.voice {
                live.recovery = Some((std::time::Instant::now(), 0));
                for call in live.phone_consents.drain(..) {
                    ctx.audio.pause_phone_withdrawal(true);
                    live.phone_withdrawals.start(call);
                }
                live.phone_announcement = None;
                live.deafened = None;
                live.announced_watches = None;
                sync_video_watches(live).await;
            }
            ctx.send(Update::Call(
                url.clone(),
                channel,
                Some(CallState::Reconnecting),
            ));
        }
        RoomEvent::Disconnected { reason } => {
            tracing::info!(?reason, "voice disconnected");
            if recoverable_disconnect(reason) {
                if let Some(live) = &mut state.voice {
                    silence_lost_transport(ctx, url, live);
                    schedule_immediate_recovery(&mut live.recovery, std::time::Instant::now());
                    live.transport_closed = true;
                }
                ctx.send(Update::Call(
                    url.clone(),
                    channel,
                    Some(CallState::Reconnecting),
                ));
                return;
            }
            // A move to another channel can follow as its own notice and replaces this text.
            if reason == DisconnectReason::ParticipantRemoved {
                state.removed_from = Some((channel, std::time::Instant::now()));
            }
            end(
                state,
                if reason == DisconnectReason::ParticipantRemoved {
                    t!("voice.removed")
                } else {
                    t!("voice.disconnected")
                },
            );
        }
        RoomEvent::ParticipantDisconnected(participant) => {
            if let Some(live) = &mut state.voice {
                if live.recovery.is_none() {
                    participant_left_watch(
                        &mut live.watched,
                        &mut live.watch_owners,
                        participant.identity().as_str(),
                    );
                }
                control::left(ctx, live, participant.identity().as_str());
                terminal::left(ctx, live, participant.identity().as_str());
            }
        }
        RoomEvent::TrackUnpublished { publication, .. } => {
            ctx.audio.set_listening(publication.sid().as_str(), false);
            ctx.audio.remove_track(publication.sid().as_str());
            if let Some(live) = &mut state.voice {
                if live.recovery.is_none() {
                    unpublish_watch(
                        &mut live.watched,
                        &mut live.watch_owners,
                        publication.sid().as_str(),
                    );
                }
                control::unpublished(ctx, live, publication.sid().as_str());
            }
        }
        RoomEvent::LocalTrackRepublished {
            previous_sid,
            publication,
            ..
        } => {
            if let Some(live) = &mut state.voice {
                #[cfg(not(any(target_os = "android", target_os = "ios")))]
                if let Some(phone) = &mut live.phone
                    && phone.sid == previous_sid
                {
                    phone.sid = publication.sid();
                }
                if let Some(share) = live
                    .shares
                    .iter_mut()
                    .find(|share| share.sid == previous_sid)
                {
                    share.sid = publication.sid();
                }
                if live.camera.as_ref() == Some(&previous_sid) {
                    live.camera = Some(publication.sid());
                }
                if let Some((sid, ..)) = &mut live.screen
                    && *sid == previous_sid
                {
                    *sid = publication.sid();
                }
            }
        }
        RoomEvent::ParticipantConnected(_)
        | RoomEvent::ParticipantActive(_)
        | RoomEvent::TrackPublished { .. }
        | RoomEvent::TrackMuted { .. }
        | RoomEvent::TrackUnmuted { .. }
        | RoomEvent::ParticipantAttributesChanged { .. }
        | RoomEvent::ConnectionStateChanged(_) => {}
        _ => return,
    }
    if let Some(live) = &mut state.voice {
        phone::sync(ctx, url, live).await;
    }
    roster(ctx, url, client, state, channel);
}

/// Stop the camera and screen once LiveKit no longer carries them: `video` was removed
/// mid-call (LiveKit drops the grant) or the SDK unpublished the track itself.
pub(super) async fn stop_lost_video(
    ctx: &Ctx,
    url: &Url,
    client: Option<&KeyClient<'_>>,
    state: &mut State,
    unpublished: Option<TrackSid>,
) {
    let Some(live) = &mut state.voice else {
        return;
    };
    let (camera, screen) = (live.room.can_use_camera(), live.room.can_share_screen());
    let gone = |sid: &TrackSid, allowed: bool| !allowed || unpublished.as_ref() == Some(sid);
    let mut changed = false;
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if live
        .phone
        .as_ref()
        .is_some_and(|phone| gone(&phone.sid, live.room.can_share_audio()))
    {
        live.phone = None;
        ctx.audio.end_phone();
        changed = true;
    }
    if live.camera.as_ref().is_some_and(|sid| gone(sid, camera)) {
        stop_camera(live).await;
        ctx.send(Update::CameraStopped);
        changed = true;
    }
    // A picker still open (`starting`) never has a published screen beside it.
    if live
        .screen
        .as_ref()
        .is_some_and(|(sid, ..)| gone(sid, screen))
        || !screen && live.starting.is_some()
    {
        changed |= stop_screen(ctx, live).await;
        ctx.send(Update::Sharing(false));
    }
    let channel = live.channel;
    if unpublished.is_none() && !(camera && screen) {
        ctx.send(Update::NoVideo(url.clone(), channel));
    }
    if changed {
        roster(ctx, url, client, state, channel);
    }
}

/// Stop sharing apps that closed or whose capture stopped by itself.
pub(super) async fn stop_finished_shares(
    ctx: &Ctx,
    url: &Url,
    client: Option<&KeyClient<'_>>,
    state: &mut State,
) {
    let Some(live) = &mut state.voice else {
        return;
    };
    let (mut ended, kept): (Vec<Share>, _) = std::mem::take(&mut live.shares)
        .into_iter()
        .partition(|share| share.capture.is_finished());
    live.shares = kept;
    let phone_ended = phone::stop_ended(ctx, live).await;
    phone::sync(ctx, url, live).await;
    if live.host.controller().is_some()
        && live
            .screen
            .as_ref()
            .is_some_and(|(_, capture, _)| !capture.supports_control())
    {
        control::stop_hosting(ctx, live).await;
    }
    if live
        .screen
        .as_ref()
        .is_some_and(|(_, capture, _)| capture.is_finished())
    {
        tracing::info!("screen capture ended");
        stop_screen(ctx, live).await;
        ctx.send(Update::Sharing(false));
    } else if ended.is_empty() && !phone_ended {
        return;
    }
    for share in &mut ended {
        tracing::info!("app audio capture ended");
        // macOS: access turned out to be off once audio flowed (it delivers silence).
        if let Some(error) = share.capture.error().filter(share::blocked) {
            ctx.send(Update::Failed(share::failed(&error, &share.app)));
        }
        if let Err(error) = live.room.unpublish_audio(&share.sid).await {
            tracing::warn!(error = %format_args!("{error:#}"), "stopping app audio failed");
        }
        if share.stream {
            live.stream_audio.retain(|app| app.id != share.app.id);
            ctx.send(Update::StreamAudio(live.stream_audio.clone()));
        } else {
            ctx.send(Update::AppStopped(share.app.id.clone()));
        }
    }
    let channel = live.channel;
    roster(ctx, url, client, state, channel);
}

/// The connection panel's numbers, from the SDK's WebRTC stats.
pub(super) async fn voice_stats(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, state: &mut State) {
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
        .map(|(sid, ..)| (sid.clone(), t!("call.track_screen").into()))
        .chain(
            live.camera
                .iter()
                .map(|sid| (sid.clone(), t!("call.track_camera").into())),
        )
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
        traffic: snapshot.traffic,
        jitter_ms: received.iter().filter_map(|t| t.jitter_ms).reduce(f64::max),
        lost_in: lost(&received),
        lost_out: lost(&sent),
        received_in: received
            .iter()
            .filter_map(|t| t.packets_received)
            .reduce(|a, b| a + b),
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
                    sid: t.sid.to_string(),
                    mine: t.outbound,
                    screen: live.screen.as_ref().is_some_and(|(sid, ..)| *sid == t.sid),
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
                    video: t.video,
                    detail: t.detail.clone(),
                    jitter_ms: t.jitter_ms,
                    lost: t.packets_lost,
                    received: t.packets_received,
                })
            })
            .collect(),
    };
    ctx.send(Update::Stats(url.clone(), stats));
}
/// Offer a file to whoever is online in a text channel or DM, shown as a message.
pub(super) async fn offer_chat_file(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: Id,
    path: PathBuf,
) -> Result<()> {
    let files =
        (state.chat_files.entry(channel)).or_insert_with(|| DataLiveFiles::new(client, channel));
    let record = match files.offer_live_file(client, &path).await {
        // The other person re-keyed the DM just now: take the new key and offer once more.
        Err(error) if super::files::conflict(&error) => {
            client.sync().await?;
            files.offer_live_file(client, &path).await?
        }
        offered => offered?,
    };
    let body = client.open_message(&channel, &record).await?;
    let file = body.live.as_ref().context("offer without a file")?;
    tracing::info!("offered a live file in a chat");
    state
        .transfers
        .entry(file.id)
        .or_insert(Transfer::chat(channel))
        .mine = true;
    ctx.send(Update::LiveOffer(LiveOffer {
        id: file.id,
        from: None,
        name: file.name.clone(),
        size: file.size,
        chat: true,
    }));
    state.sent(ctx, url, channel, &record, &body);
    Ok(())
}

/// Receive a live file offered in a text channel or DM into this path.
pub(super) async fn accept_chat_file(
    ctx: &Ctx,
    client: &mut KeyClient<'_>,
    state: &mut State,
    id: Id,
    path: PathBuf,
) -> Result<()> {
    let Some((channel, Some(record))) =
        (state.transfers.get(&id)).map(|transfer| (transfer.channel, transfer.offer.clone()))
    else {
        return Ok(());
    };
    let files =
        (state.chat_files.entry(channel)).or_insert_with(|| DataLiveFiles::new(client, channel));
    let download = match files.accept_live_file(client, &record, &path).await {
        Ok(download) => download,
        Err(error) => {
            let reason = format!("{error:#}");
            ctx.send(Update::LiveProgress(id, LiveFileEvent::Failed { reason }));
            return Err(error);
        }
    };
    tracing::info!("receiving a live file in a chat");
    if let Some(transfer) = state.transfers.get_mut(&id) {
        transfer.download = Some(follow(ctx, id, download));
    }
    Ok(())
}

/// Show how a live download goes until it ends.
fn follow(ctx: &Ctx, id: Id, mut download: LiveDownload) -> tokio::task::JoinHandle<()> {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut shown = std::time::Instant::now();
        while let Some(event) = download.next_event().await {
            let end = !matches!(event, LiveFileEvent::Progress { .. });
            // Progress comes per chunk; a few updates a second are enough to draw.
            if end || shown.elapsed() > Duration::from_millis(150) {
                shown = std::time::Instant::now();
                ctx.send(Update::LiveProgress(id, event));
            }
            if end {
                break;
            }
        }
    })
}

/// Receive an offered live file into this path.
pub(super) async fn accept_file(
    ctx: &Ctx,
    client: &mut KeyClient<'_>,
    state: &mut State,
    id: Id,
    path: PathBuf,
) -> Result<()> {
    let Some(live) = &mut state.voice else {
        return Ok(());
    };
    let Some(transfer) = state.transfers.get_mut(&id) else {
        return Ok(());
    };
    let Some(record) = transfer.offer.take() else {
        return Ok(());
    };
    let download = match live.room.accept_live_file(client, &record, &path).await {
        Ok(download) => download,
        Err(error) => {
            let reason = format!("{error:#}");
            ctx.send(Update::LiveProgress(id, LiveFileEvent::Failed { reason }));
            return Err(error);
        }
    };
    tracing::info!("receiving a live file");
    transfer.download = Some(follow(ctx, id, download));
    Ok(())
}

/// Leave any call on this server and join this voice channel.
pub(super) async fn start_call(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    channel: Id,
) -> Result<()> {
    leave_voice(ctx, url, client, state);
    match client.prepare_voice(&channel).await {
        Ok(pending) => {
            state.joining = Some(spawn_join(ctx, pending, channel));
            Ok(())
        }
        Err(error) => {
            ctx.send(Update::Call(url.clone(), channel, None));
            Err(error)
        }
    }
}

/// The join `start_call` began is done: show the call, or that it failed.
pub(super) async fn finish_call(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
    state: &mut State,
    channel: Id,
    joined: Result<VoiceRoom>,
) -> Result<()> {
    let room = match joined {
        Ok(room) => room,
        Err(error) => {
            ctx.send(Update::Call(url.clone(), channel, None));
            return Err(error);
        }
    };
    tracing::info!(server = %fingerprint(&state.server_id), "joined voice");
    let listening = !room.can_publish();
    if listening {
        ctx.send(Update::Failed(t!("voice.listen_only").into()));
    }
    // Mute or deafen may have changed while publishing.
    room.set_microphone_muted(ctx.audio.muted() || ctx.audio.deafened());
    let no_video = !room.can_use_camera();
    state.voice = Some(Live {
        recovery: None,
        transport_closed: false,
        camera_source: None,
        watch_owners: HashMap::new(),
        room,
        channel,
        deafened: Some(false),
        shares: Vec::new(),
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        phone: None,
        phone_consents: Vec::new(),
        phone_announcement: None,
        phone_withdrawals: Default::default(),
        phone_acknowledgements: Vec::new(),
        pending: Vec::new(),
        stream_audio: Vec::new(),
        apps: HashMap::new(),
        nameless: HashMap::new(),
        starting: None,
        screen: None,
        swap: screen::Swap::Idle,
        camera: None,
        watched: HashSet::new(),
        announced_watches: Some(Vec::new()),
        host: Host::default(),
        guest: Guest::default(),
        control: ControlView::default(),
        terminal: None,
        viewing: None,
        rang: None,
    });
    ctx.send(Update::Call(
        url.clone(),
        channel,
        Some(CallState::Connected),
    ));
    if listening {
        ctx.send(Update::CanTalk(url.clone(), channel, false));
    }
    if no_video {
        ctx.send(Update::NoVideo(url.clone(), channel));
    }
    // Key events handled while connecting may have rotated the channel key since the token.
    sync_voice_key(ctx, client, state);
    send_room(ctx, url, client, state, channel);
    sync_voice_status(ctx, state).await;
    Ok(())
}

/// Publish an app's audio as its own track once its capture starts.
pub(super) fn share_app(ctx: &Ctx, state: &mut State, app: App) {
    // While live, sound goes with the stream instead (see `set_stream_audio`).
    let Some(live) = state
        .voice
        .as_mut()
        .filter(|live| live.screen.is_none() && live.room.can_share_audio())
    else {
        ctx.send(Update::AppStopped(app.id));
        return;
    };
    if !live.pending.iter().any(|s| !s.stream && s.app.id == app.id) {
        start_share(live, app, false);
    }
}

impl Live {
    pub(super) fn stop_audio_captures(&mut self) {
        for start in self.pending.drain(..) {
            start.task.abort();
        }
        self.shares.clear();
        self.stream_audio.clear();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            self.phone = None;
        }
    }
}

/// Start a screen capture; `share_screen` publishes it once it started.
pub(super) fn pick_screen(
    ctx: &Ctx,
    state: &mut State,
    target: screen::Target,
    options: screen::Options,
    audio: Vec<App>,
) -> Result<()> {
    let Some(live) = state.voice.as_mut().filter(|live| live.screen.is_none()) else {
        ctx.send(Update::Sharing(false));
        return Ok(());
    };
    if live.room.can_share_audio() {
        live.stream_audio = audio;
    } else {
        live.stream_audio.clear();
        ctx.send(Update::StreamAudio(Vec::new()));
    }
    let preview = ctx.video.preview.clone();
    // The system picker waits for the person; keep the server loop running meanwhile.
    live.starting = Some(tokio::spawn(async move {
        // shortcut: always opens a fresh picker; keep the portal's restore token
        // if people ask to skip it when sharing the same screen again. The
        // "share the window I'm using" shortcut relies on the picker asking: with
        // a token it would share the last pick without a word.
        let capture = screen::start(target, options, &mut None, preview).await?;
        Ok((capture, options))
    }));
    Ok(())
}

/// Pick another source for the screen we share. Its frames go into the published
/// track's own source, so viewers keep watching with no reconnect.
fn change_screen(ctx: &Ctx, live: &mut Live, target: screen::Target, remote_control: bool) {
    let Some((_, capture, options)) = &live.screen else {
        return;
    };
    let (into, mut options) = (capture.source.clone(), *options);
    options.remote_control = remote_control;
    let preview = ctx.video.preview.clone();
    // No restore token, so the shortcut's picker always asks (see `pick_screen`).
    let picker = tokio::spawn(async move {
        screen::start_into(target, options, &mut None, Some(into), preview).await
    });
    if let Err(picker) = live.swap.begin(picker) {
        picker.abort();
        ctx.send(Update::Sharing(true));
    }
}

/// The picker for another source finished: switch to it, or keep the old share.
pub(super) fn screen_changed(ctx: &Ctx, state: &mut State, result: Result<screen::Capture>) {
    let Some(live) = &mut state.voice else {
        return;
    };
    match live.swap.finish(result) {
        screen::Outcome::Swapped(new) => {
            if let Some((_, capture, _)) = &mut live.screen {
                tracing::info!("changed the shared screen");
                // Dropping the old capture stops it; the new one already feeds the track.
                *capture = new;
            }
        }
        screen::Outcome::Cancelled => tracing::info!("screen change cancelled"),
        screen::Outcome::Failed(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "changing the shared screen failed");
            ctx.send(Update::Failed(t!("voice.switch_failed").into()));
        }
    }
    if live.screen.is_some() {
        ctx.send(Update::Sharing(true));
    }
}

/// Publish a running camera, replacing an earlier one.
pub(super) async fn share_camera(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
    state: &mut State,
    source: NativeVideoSource,
    options: CameraOptions,
) -> Result<()> {
    let Some(live) = &mut state.voice else {
        ctx.send(Update::CameraStopped);
        return Ok(());
    };
    stop_camera(live).await;
    match live.room.publish_camera(options, source.clone()).await {
        Ok(publication) => {
            tracing::info!("sharing camera");
            live.camera = Some(publication.sid());
            live.camera_source = Some((source, options));
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "sharing camera failed");
            ctx.send(Update::CameraStopped);
            ctx.send(Update::Failed(t!("voice.camera_failed").into()));
        }
    }
    let channel = live.channel;
    send_room(ctx, url, client, state, channel);
    Ok(())
}

/// Someone accepted (or cancelled) our live file: tell the room, and show who receives it.
pub(super) fn live_file_accepted(
    ctx: &Ctx,
    client: &KeyClient<'_>,
    state: &mut State,
    event: &Event,
) -> Result<()> {
    let Event::LiveFileAccepted {
        channel,
        device,
        acceptance,
        data,
    } = event
    else {
        return Ok(());
    };
    let mine = if *data {
        let Some(files) = state.chat_files.get_mut(&decode::<32>(channel)?) else {
            return Ok(());
        };
        files.handle_live_file_event(client, event)?;
        true
    } else if let Some(voice) = &mut state.voice {
        voice.room.handle_live_file_event(client, event)?;
        (state.transfers.get(&decode(&acceptance.transfer)?))
            .is_some_and(|transfer| transfer.mine && !transfer.chat)
    } else {
        false
    };
    let person = client
        .devices()
        .find(|cert| encode(cert.signing_key().as_bytes()) == *device)
        .map(|cert| *cert.person_id());
    if let Some(person) = person
        && mine
    {
        let transfer = decode(&acceptance.transfer)?;
        ctx.send(Update::LiveReceiver(transfer, person, !acceptance.cancel));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn retries_transport_failure_but_never_a_removal_or_duplicate_session() {
        for reason in [
            DisconnectReason::ServerShutdown,
            DisconnectReason::SignalClose,
            DisconnectReason::ConnectionTimeout,
            DisconnectReason::MediaFailure,
            DisconnectReason::JoinFailure,
        ] {
            assert!(recoverable_disconnect(reason));
        }
        for reason in [
            DisconnectReason::ParticipantRemoved,
            DisconnectReason::DuplicateIdentity,
            DisconnectReason::ClientInitiated,
            DisconnectReason::RoomDeleted,
            DisconnectReason::UserRejected,
        ] {
            assert!(!recoverable_disconnect(reason));
        }
    }

    #[test]
    fn recovery_backoff_ends_instead_of_reconnecting_forever() {
        assert_eq!(
            (0..6).map(recovery_delay).collect::<Vec<_>>(),
            [Some(1), Some(2), Some(4), Some(8), Some(16), None]
                .map(|seconds| seconds.map(Duration::from_secs))
        );
        assert_eq!(recovery_delay(u32::MAX), None);
    }

    #[tokio::test]
    async fn a_connecting_join_holds_call_commands_and_reports_a_lost_task() {
        assert!(waits_for_call(&Command::StopScreen));
        assert!(waits_for_call(&Command::ShareApp(share::system())));
        assert!(!waits_for_call(&Command::LeaveVoice));
        assert!(!waits_for_call(&Command::JoinVoice([2; 32])));

        let (done, room) = tokio::sync::oneshot::channel();
        let mut joining = Some(Joining {
            channel: [1; 32],
            room,
            deferred: Vec::new(),
        });
        drop(done);
        assert!(next_join(&mut joining).await.is_err());
    }

    #[test]
    fn immediate_recovery_clears_reconnecting_grace_but_keeps_active_backoff() {
        let now = std::time::Instant::now();
        let later = now + Duration::from_secs(10);
        let mut recovery = Some((later, 0));
        schedule_immediate_recovery(&mut recovery, now);
        assert_eq!(recovery, Some((now, 0)));

        let mut backoff = Some((later, 2));
        schedule_immediate_recovery(&mut backoff, now);
        assert_eq!(backoff, Some((later, 2)));

        let mut none = None;
        schedule_immediate_recovery(&mut none, now);
        assert_eq!(none, Some((now, 0)));
    }

    #[test]
    fn camera_remap_and_old_unwatch_keep_replacement_subscribed() {
        let mut watched = HashSet::new();
        let mut owners = WatchOwners::new();
        watch_on(
            &mut watched,
            &mut owners,
            "cam-old",
            Some(("ann".into(), TrackSource::Camera)),
        );
        let replaced = remap_watch(
            &mut watched,
            &mut owners,
            "ann",
            TrackSource::Camera,
            "cam-new",
        );
        assert_eq!(replaced, Some(vec![("cam-old".into(), "cam-new".into())]));
        assert!(watched.contains("cam-new") && !watched.contains("cam-old"));

        // UI learned the new camera from Room before VideoReplaced: Watch(new, true) then Watch(old, false).
        watch_on(
            &mut watched,
            &mut owners,
            "cam-new",
            Some(("ann".into(), TrackSource::Camera)),
        );
        let dropped = watch_off(&mut watched, &mut owners, "cam-old");
        assert_eq!(dropped, vec!["cam-old"]);
        assert!(watched.contains("cam-new"));

        // Explicit unwatch of the old SID without a new watch still unsubscribes the remapped track.
        let mut watched = HashSet::new();
        let mut owners = WatchOwners::new();
        watch_on(
            &mut watched,
            &mut owners,
            "scr-old",
            Some(("ann".into(), TrackSource::Screenshare)),
        );
        remap_watch(
            &mut watched,
            &mut owners,
            "ann",
            TrackSource::Screenshare,
            "scr-new",
        );
        let dropped = watch_off(&mut watched, &mut owners, "scr-old");
        assert!(dropped.contains(&"scr-new".to_owned()));
        assert!(!watched.contains("scr-new"));
    }

    #[test]
    fn normal_unpublish_and_departure_clear_watch_owners_so_later_shares_stay_opt_in() {
        let mut watched = HashSet::new();
        let mut owners = WatchOwners::new();
        watch_on(
            &mut watched,
            &mut owners,
            "scr-1",
            Some(("ann".into(), TrackSource::Screenshare)),
        );
        unpublish_watch(&mut watched, &mut owners, "scr-1");
        assert!(watched.is_empty() && owners.is_empty());
        assert_eq!(
            remap_watch(
                &mut watched,
                &mut owners,
                "ann",
                TrackSource::Screenshare,
                "scr-2"
            ),
            None
        );

        watch_on(
            &mut watched,
            &mut owners,
            "scr-2",
            Some(("ann".into(), TrackSource::Screenshare)),
        );
        participant_left_watch(&mut watched, &mut owners, "ann");
        assert!(watched.is_empty() && owners.is_empty());
    }

    #[test]
    fn screen_counts_people_per_track_and_excludes_the_sharer() {
        let mut watches = HashSet::from([
            ([1; 32], "screen-a".into()),
            ([2; 32], "screen-a".into()),
            ([2; 32], "screen-a".into()),
            ([3; 32], "screen-a".into()),
            ([2; 32], "screen-b".into()),
            ([4; 32], "camera".into()),
        ]);
        assert_eq!(screen_viewers(&watches, &[1; 32], "screen-a"), 2);
        assert_eq!(screen_viewers(&watches, &[3; 32], "screen-b"), 1);
        assert_eq!(screen_viewers(&watches, &[2; 32], "screen-b"), 0);
        watches.retain(|(person, _)| *person != [3; 32]);
        assert_eq!(screen_viewers(&watches, &[1; 32], "screen-a"), 1);
        watches.clear();
        assert_eq!(screen_viewers(&watches, &[1; 32], "screen-a"), 0);
    }

    use rusttalk_core::client::voice::VoiceRoster;

    use super::*;

    #[test]
    fn leaving_drops_us_from_the_old_channel_until_the_server_says_otherwise() {
        let (me, jakob, old, other) = ([1; 32], [2; 32], [10; 32], [11; 32]);
        let mut state = State::default();
        let roster = VoiceRoster {
            people: vec![jakob, me],
            unknown: 1,
        };
        state.rooms.insert(old, roster.clone());
        state.rooms.insert(other, roster);
        left_room(&mut state, &old, &me);
        assert_eq!(state.rooms[&old].people, [jakob]);
        assert_eq!(state.rooms[&old].unknown, 1);
        // Only the channel we left changes.
        assert_eq!(state.rooms[&other].people, [jakob, me]);
    }
}
