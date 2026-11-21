//! The voice room: joining, sharing apps, screens, cameras and live files, and who is in it.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use livekit::prelude::RemoteTrack;
use rusttalk_core::{
    auth::{decode, encode, voice_identity},
    camera::CameraOptions,
    client::{
        Event, KeyClient, MessageRecord,
        voice::{
            ConnectionState, DataLiveFiles, LiveDownload, LiveFileEvent, NativeVideoSource,
            RemoteParticipant, RoomEvent, TrackSid, TrackSource, VoiceRoom, VoiceState,
            VoiceStatus,
        },
    },
    control::{Guest, Host},
    identity::fingerprint,
};
use url::Url;

use super::{
    Command, Ctx, LiveOffer, State, Update,
    control::{self, ControlView},
    send_messages, to_msg,
};
use crate::{
    screen,
    server::{CallState, Id, Peer, Screen, Stats, TrackStats},
    share::{self, App, Capture},
    sounds::Snapshot,
};

/// The voice room this device is in, on this server.
pub(super) struct Live {
    pub(super) room: VoiceRoom,
    pub(super) channel: Id,
    /// The last acknowledged deafen state, so failures can be retried.
    deafened: Option<bool>,
    /// Apps this device shares, each with its own track.
    shares: Vec<Share>,
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
    swap: screen::Swap<tokio::task::JoinHandle<Result<screen::Capture>>>,
    /// The published camera; the UI owns its capture.
    camera: Option<TrackSid>,
    /// Screen and camera tracks this device receives; others are unsubscribed.
    watched: HashSet<String>,
    announced_watches: Option<Vec<TrackSid>>,
    /// Who asks for, or has, control of our screen.
    pub(super) host: Host,
    /// The screen we ask for, or have, control of.
    pub(super) guest: Guest,
    /// What the window last heard about both.
    pub(super) control: ControlView,
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

pub(super) struct Share {
    app: App,
    sid: TrackSid,
    /// Stops capturing when dropped.
    capture: Capture,
}

/// Commands that only need the voice room, which outlives the API connection.
pub(super) fn voice_only(command: &Command) -> bool {
    matches!(
        command,
        Command::LeaveVoice
            | Command::SyncVoiceStatus
            | Command::StopApp(_)
            | Command::StopScreen
            | Command::ChangeScreen(_)
            | Command::Watch(..)
            | Command::StopCamera
            | Command::CancelFile(_)
    ) || control::is_control(command)
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
    let changed = match command {
        Command::StopApp(id) => {
            let Some(at) = live.shares.iter().position(|share| share.app.id == id) else {
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
        Command::ChangeScreen(target) => {
            // Control was granted for what is shared now, not what comes next.
            control::stop_hosting(ctx, live).await;
            change_screen(ctx, live, target);
            false
        }
        Command::Watch(sid, on) => {
            watch(ctx, live, sid, on)?;
            sync_video_watches(live).await;
            false
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
    let Some((sid, ..)) = live.screen.take() else {
        return false;
    };
    tracing::info!("stopped sharing screen");
    if let Err(error) = live.room.unpublish_video(&sid).await {
        tracing::warn!(error = %format_args!("{error:#}"), "stopping screen failed");
    }
    true
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

/// Connect to a voice channel and publish the microphone when allowed to talk.
pub(super) async fn join_voice(
    ctx: &Ctx,
    client: &mut KeyClient<'_>,
    channel: &Id,
) -> Result<VoiceRoom> {
    let room = client.join_voice(channel).await?;
    if room.can_publish() {
        if let Err(error) = room.publish_audio(ctx.audio.source()).await {
            leave_room(room).await;
            return Err(error);
        }
    } else {
        ctx.send(Update::Failed(t!("voice.listen_only").into()));
    }
    room.set_microphone_muted(ctx.audio.muted() || ctx.audio.deafened());
    Ok(room)
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
    let mut live = state.voice.take()?;
    control::leave(&mut live);
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

pub(super) async fn leave_room(room: VoiceRoom) {
    tracing::info!("left voice");
    if let Err(error) = room.leave().await {
        tracing::warn!(error = %format_args!("{error:#}"), "leaving voice failed");
    }
}

/// Install a newer media key after key work for the voice channel.
pub(super) fn sync_voice_key(client: &KeyClient<'_>, state: &mut State) {
    if let Some(live) = &mut state.voice
        && let Err(error) = live.room.sync_key(client)
    {
        tracing::warn!(error = %format_args!("{error:#}"), "voice key rotation failed");
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
}

pub(super) async fn next_voice(voice: &mut Option<Live>) -> Next {
    let Some(live) = voice else {
        return std::future::pending().await;
    };
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
    tokio::select! {
        event = live.room.next_event() => Next::Event(event),
        capture = starting => Next::Capture(capture.map_err(anyhow::Error::from).and_then(|c| c)),
        capture = swapping => Next::Swap(capture.map_err(anyhow::Error::from).and_then(|c| c)),
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
        })),
        Some(live) => {
            let mut live_people: HashMap<Id, bool> = HashMap::new();
            let mut statuses: HashMap<Id, VoiceStatus> = HashMap::new();
            let mut apps: HashMap<Id, Vec<(TrackSid, String)>> = HashMap::new();
            let mut screens: HashMap<Id, Vec<Screen>> = HashMap::new();
            let mut cameras: HashMap<Id, String> = HashMap::new();
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
                    if *person != me && voice != VoiceState::Disconnected {
                        sounds.people.insert(*person);
                    }
                    if voice == VoiceState::Connected {
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
                let Some(person) = ids.get(video.participant.as_str()) else {
                    continue;
                };
                match video.source {
                    TrackSource::Screenshare => screens.entry(*person).or_default().push(Screen {
                        sid: video.sid.to_string(),
                        name: video.name,
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
            if let Some((sid, ..)) = &live.screen {
                screens.insert(
                    me,
                    vec![Screen {
                        sid: sid.to_string(),
                        name: t!("call.your_screen").into(),
                    }],
                );
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
        end(state, t!("voice.disconnected"));
        roster(ctx, url, client, state, channel);
        return;
    };
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
                    live.apps.insert(sid.clone(), name);
                    app = Some(sid.clone());
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
            tracing::info!("voice reconnecting");
            ctx.send(Update::Call(
                url.clone(),
                channel,
                Some(CallState::Reconnecting),
            ));
        }
        RoomEvent::Reconnected => {
            tracing::info!("voice reconnected");
            if let Some(live) = &mut state.voice {
                live.deafened = None;
                live.announced_watches = None;
                sync_video_watches(live).await;
            }
            ctx.send(Update::Call(
                url.clone(),
                channel,
                Some(CallState::Connected),
            ));
        }
        RoomEvent::Disconnected { reason } => {
            tracing::info!(?reason, "voice disconnected");
            end(state, t!("voice.disconnected"));
        }
        RoomEvent::ParticipantDisconnected(participant) => {
            if let Some(live) = &mut state.voice {
                control::left(ctx, live, participant.identity().as_str());
            }
        }
        RoomEvent::TrackUnpublished { publication, .. } => {
            if let Some(live) = &mut state.voice {
                control::unpublished(ctx, live, publication.sid().as_str());
            }
        }
        RoomEvent::LocalTrackRepublished {
            previous_sid,
            publication,
            ..
        } => {
            if let Some(live) = &mut state.voice {
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
    let (ended, kept): (Vec<Share>, _) = std::mem::take(&mut live.shares)
        .into_iter()
        .partition(|share| share.capture.is_finished());
    live.shares = kept;
    if live
        .screen
        .as_ref()
        .is_some_and(|(_, capture, _)| capture.is_finished())
    {
        tracing::info!("screen capture ended");
        stop_screen(ctx, live).await;
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
                    video: t.video,
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
    send_messages(ctx, url, channel, vec![to_msg(&record, &body)], false, None);
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
    let room = match join_voice(ctx, client, &channel).await {
        Ok(room) => room,
        Err(error) => {
            ctx.send(Update::Call(url.clone(), channel, None));
            return Err(error);
        }
    };
    tracing::info!(server = %fingerprint(&state.server_id), "joined voice");
    let listening = !room.can_publish();
    let no_video = !room.can_use_camera();
    state.voice = Some(Live {
        room,
        channel,
        deafened: Some(false),
        shares: Vec::new(),
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
    });
    ctx.send(Update::Call(
        url.clone(),
        channel,
        Some(CallState::Connected),
    ));
    if listening {
        ctx.send(Update::Listening(url.clone(), channel));
    }
    if no_video {
        ctx.send(Update::NoVideo(url.clone(), channel));
    }
    send_room(ctx, url, client, state, channel);
    sync_voice_status(ctx, state).await;
    Ok(())
}

/// Publish an app's audio as its own track.
pub(super) async fn share_app(
    ctx: &Ctx,
    url: &Url,
    client: &KeyClient<'_>,
    state: &mut State,
    app: App,
) -> Result<()> {
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
            let text = t!("voice.app_failed", app = app.name);
            ctx.send(Update::AppStopped(app.id));
            ctx.send(Update::Failed(text));
        }
    }
    Ok(())
}

/// Start a screen capture; `share_screen` publishes it once it started.
pub(super) fn pick_screen(
    ctx: &Ctx,
    state: &mut State,
    target: screen::Target,
    options: screen::Options,
) -> Result<()> {
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
    Ok(())
}

/// Pick another source for the screen we share. Its frames go into the published
/// track's own source, so viewers keep watching with no reconnect.
fn change_screen(ctx: &Ctx, live: &mut Live, target: screen::Target) {
    let Some((_, capture, options)) = &live.screen else {
        return;
    };
    let (into, options) = (capture.source.clone(), *options);
    let picker =
        tokio::spawn(
            async move { screen::start_into(target, options, &mut None, Some(into)).await },
        );
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
    match live.room.publish_camera(options, source).await {
        Ok(publication) => {
            tracing::info!("sharing camera");
            live.camera = Some(publication.sid());
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
