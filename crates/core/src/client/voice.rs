//! Pinned, end-to-end encrypted voice sessions and caller-owned audio I/O.

mod live_file;
mod screen;
mod stats;
mod track_name;

use std::{collections::HashMap, sync::Arc};

use anyhow::{Context, Result, ensure};
use hkdf::Hkdf;
use livekit::{
    Room, RoomDataStreamOptions, RoomOptions,
    e2ee::{
        E2eeOptions, EncryptionType,
        key_provider::{KeyProvider, KeyProviderOptions},
    },
    options::TrackPublishOptions,
    participant::ParticipantState,
    prelude::{LocalTrack, RemoteTrack, RemoteTrackPublication, TrackKind},
    rtc_engine::SimulateScenario,
    webrtc::audio_source::RtcAudioSource,
};
use reqwest::Method;
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::mpsc;
use zeroize::Zeroizing;

pub use livekit::{
    ConnectionState, RoomEvent,
    prelude::{
        LocalTrackPublication, ParticipantIdentity, RemoteAudioTrack, RemoteParticipant, TrackSid,
        TrackSource,
    },
    webrtc::{audio_source::native::NativeAudioSource, audio_stream::native::NativeAudioStream},
};

use crate::{
    auth::{KnownServer, decode, encode},
    keys::{Key, Kind, Scope},
    pinned_tls,
};

use super::KeyClient;
pub use live_file::{DataLiveFiles, LiveDownload, LiveFileEvent};
pub use screen::{NativeVideoSource, NativeVideoStream, RemoteVideo, RemoteVideoTrack};
pub use stats::{TrackStats, VoiceStats};

/// Derive a 32-byte media key for one server, channel and channel-key epoch.
pub fn media_key(channel_key: &Key, server: &[u8; 32], channel: &[u8; 32], epoch: i64) -> Key {
    let mut key = Zeroizing::new([0; 32]);
    Hkdf::<Sha256>::new(None, channel_key.as_ref())
        .expand_multi_info(
            &[b"rusttalk-media-v1", server, channel, &epoch.to_be_bytes()],
            key.as_mut(),
        )
        .expect("32-byte HKDF-SHA256 output");
    key
}

/// LiveKit frame key slot for an epoch.
pub fn key_index(epoch: i64) -> i32 {
    epoch.rem_euclid(16) as i32
}

/// State to display while joining and for each participant in a joined room.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceState {
    Connecting,
    Connected,
    Reconnecting,
    Disconnected,
}

/// People in a voice room, plus devices the server could not match to a current reader.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VoiceRoster {
    pub people: Vec<[u8; 32]>,
    /// Show these as "unknown device".
    pub unknown: usize,
}

#[derive(Deserialize)]
struct Roster {
    people: Vec<String>,
    unknown: usize,
}

#[derive(Deserialize)]
struct Token {
    token: String,
}

fn scope(id: &[u8; 32]) -> Scope {
    Scope {
        kind: Kind::Channel,
        id: *id,
    }
}

fn signaling_url(server: &KnownServer) -> Result<reqwest::Url> {
    let mut url = server.address.clone();
    url.set_scheme("wss")
        .map_err(|_| anyhow::anyhow!("invalid LiveKit signaling origin"))?;
    // Unbox so callers can downcast to `PinnedElsewhere`.
    pinned_tls::register(&url, server.tls_fingerprint).map_err(|error| {
        match error.downcast::<pinned_tls::PinnedElsewhere>() {
            Ok(error) => anyhow::Error::new(*error),
            Err(error) => anyhow::Error::from_boxed(error),
        }
    })?;
    Ok(url)
}

impl KeyClient<'_> {
    /// Reconcile keys, request a scoped token, and connect with the current epoch.
    pub async fn join_voice(&mut self, channel: &[u8; 32]) -> Result<VoiceRoom> {
        self.sync().await?;
        let (epoch, key) = self.current_key(scope(channel))?;
        let reply: Token = self
            .request(
                Method::POST,
                &format!("/v1/voice/{}/token", encode(channel)),
            )
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let server = self.server.known_server();
        let url = signaling_url(server)?;
        let name_keys = self.name_keys(channel, track_name::APP_AUDIO);
        let screen_names = self.name_keys(channel, track_name::SCREEN);
        let key = media_key(key, &server.server_id, channel, epoch);
        let provider = KeyProvider::with_shared_key(
            KeyProviderOptions {
                ratchet_window_size: 0,
                ..Default::default()
            },
            key.to_vec(),
        );
        install_key(&provider, &key, epoch)?;
        let mut options = RoomOptions::default();
        options.encryption = Some(E2eeOptions {
            key_provider: provider.clone(),
            encryption_type: EncryptionType::Gcm,
        });
        options.join_retries = 0;
        options.auto_subscribe = false;
        options.data_stream =
            RoomDataStreamOptions::default().with_max_payload_byte_length(live_file::RECEIVE_CAP);
        let (room, events) = Room::connect(url.as_str(), &reply.token, options)
            .await
            .context("connect to pinned LiveKit room")?;
        for participant in room.remote_participants().into_values() {
            for publication in participant.track_publications().into_values() {
                subscribe_audio(&publication);
            }
        }
        Ok(VoiceRoom {
            room: Arc::new(room),
            events,
            server: server.clone(),
            channel: *channel,
            epoch,
            provider,
            name_keys,
            screen_names,
            stats: stats::Sampler::default(),
            files: live_file::LiveFiles::default(),
        })
    }

    // Name keys for every held epoch, so app names published before a rotation still decrypt.
    fn name_keys(&self, channel: &[u8; 32], label: &[u8]) -> HashMap<i64, Key> {
        let server = &self.server.known_server().server_id;
        self.epoch_keys(scope(channel))
            .map(|(epoch, key)| {
                let media = media_key(key, server, channel, epoch);
                (epoch, track_name::derive(&media, label))
            })
            .collect()
    }

    /// Fetch who is currently in a voice channel.
    pub async fn voice_participants(&self, channel: &[u8; 32]) -> Result<VoiceRoster> {
        let roster: Roster = self
            .request(
                Method::GET,
                &format!("/v1/voice/{}/participants", encode(channel)),
            )
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(VoiceRoster {
            people: roster
                .people
                .iter()
                .map(|id| decode(id))
                .collect::<Result<_, _>>()?,
            unknown: roster.unknown,
        })
    }
}

// Every slot holds the current key. Frames stamped with a retired epoch's slot then fail
// decryption, so a removed sender cannot be heard, and new SDK senders (index zero) start
// on the current key. The primary slot is written last because the provider tracks it.
fn install_key(provider: &KeyProvider, key: &Key, epoch: i64) -> Result<()> {
    let index = key_index(epoch);
    for slot in (0..16).filter(|slot| *slot != index).chain([index]) {
        ensure!(
            provider.set_shared_key(key.to_vec(), slot),
            "LiveKit rejected media key slot {slot}"
        );
    }
    Ok(())
}

/// A joined voice channel. Use the API events to reconcile keys alongside room events.
pub struct VoiceRoom {
    room: Arc<Room>,
    events: mpsc::UnboundedReceiver<RoomEvent>,
    server: KnownServer,
    channel: [u8; 32],
    epoch: i64,
    provider: KeyProvider,
    name_keys: HashMap<i64, Key>,
    screen_names: HashMap<i64, Key>,
    stats: stats::Sampler,
    files: live_file::LiveFiles,
}

fn subscribe_audio(publication: &RemoteTrackPublication) {
    if publication.kind() == TrackKind::Audio
        && publication.encryption_type() == EncryptionType::Gcm
        && !publication.is_desired()
    {
        publication.set_subscribed(true);
    }
}

impl VoiceRoom {
    /// Current installed channel-key epoch.
    pub fn epoch(&self) -> i64 {
        self.epoch
    }

    /// Local device identity used by LiveKit (see `auth::voice_identity`).
    pub fn identity(&self) -> ParticipantIdentity {
        self.room.local_participant().identity()
    }

    /// Whether this device's LiveKit grant allows microphone publication.
    pub fn can_publish(&self) -> bool {
        self.can_publish_source(livekit_protocol::TrackSource::Microphone)
    }

    fn can_publish_source(&self, source: livekit_protocol::TrackSource) -> bool {
        self.room.local_participant().permission().is_some_and(|p| {
            p.can_publish
                && (p.can_publish_sources.is_empty()
                    || p.can_publish_sources.contains(&(source as i32)))
        })
    }

    /// Install a newer verified channel key after KeyClient reconciles an API event.
    pub fn sync_key(&mut self, keys: &KeyClient<'_>) -> Result<bool> {
        ensure!(
            &self.server == keys.server.known_server(),
            "key client belongs to a different server"
        );
        let (epoch, key) = keys.current_key(scope(&self.channel))?;
        self.name_keys = keys.name_keys(&self.channel, track_name::APP_AUDIO);
        self.screen_names = keys.name_keys(&self.channel, track_name::SCREEN);
        if epoch <= self.epoch {
            return Ok(false);
        }
        let key = media_key(key, &self.server.server_id, &self.channel, epoch);
        install_key(&self.provider, &key, epoch)?;
        self.epoch = epoch;
        self.switch_senders();
        Ok(true)
    }

    fn switch_senders(&self) {
        let local = self.identity();
        for ((identity, _), cryptor) in self.room.e2ee_manager().frame_cryptors() {
            if identity == local {
                cryptor.set_key_index(key_index(self.epoch));
            }
        }
    }

    /// Next SDK event. Encrypted subscribed audio tracks arrive as `TrackSubscribed`.
    pub async fn next_event(&mut self) -> Option<RoomEvent> {
        while let Some(event) = self.events.recv().await {
            #[allow(deprecated, reason = "discard the SDK's legacy raw stream events")]
            match &event {
                RoomEvent::TokenRefreshed { .. } => continue,
                RoomEvent::ByteStreamOpened {
                    reader,
                    participant_identity,
                    ..
                } => {
                    if let Some(reader) = reader.take() {
                        self.files.receive(participant_identity, reader);
                    }
                    continue;
                }
                RoomEvent::Disconnected { .. } => self.files.disconnect(),
                // Only the file protocol authenticates its data payloads independently.
                RoomEvent::DataReceived { .. }
                | RoomEvent::ChatMessage { .. }
                | RoomEvent::TranscriptionReceived { .. }
                | RoomEvent::SipDTMFReceived { .. }
                | RoomEvent::TextStreamOpened { .. }
                | RoomEvent::StreamHeaderReceived { .. }
                | RoomEvent::StreamChunkReceived { .. }
                | RoomEvent::StreamTrailerReceived { .. } => continue,
                RoomEvent::TrackSubscribed { publication, .. }
                    if publication.encryption_type() != EncryptionType::Gcm =>
                {
                    publication.set_subscribed(false);
                    tracing::warn!("refused unencrypted voice track");
                    continue;
                }
                RoomEvent::TrackPublished { publication, .. } => subscribe_audio(publication),
                RoomEvent::LocalTrackPublished { .. } | RoomEvent::LocalTrackRepublished { .. } => {
                    self.switch_senders();
                }
                _ => (),
            }
            return Some(event);
        }
        None
    }

    /// Current local connection state.
    pub fn connection_state(&self) -> ConnectionState {
        self.room.connection_state()
    }

    /// Remote participants and their current display state, including local reconnects.
    pub fn participants(&self) -> Vec<(RemoteParticipant, VoiceState)> {
        let local = self.connection_state();
        self.room
            .remote_participants()
            .into_values()
            .map(|participant| {
                let state = match (local, participant.state()) {
                    (ConnectionState::Disconnected, _) | (_, ParticipantState::Disconnected) => {
                        VoiceState::Disconnected
                    }
                    (ConnectionState::Reconnecting, _) => VoiceState::Reconnecting,
                    (_, ParticipantState::Active) => VoiceState::Connected,
                    _ => VoiceState::Connecting,
                };
                (participant, state)
            })
            .collect()
    }

    /// Snapshot of subscribed encrypted audio tracks, grouped by device identity.
    pub fn remote_audio_tracks(&self) -> Vec<(ParticipantIdentity, TrackSid, RemoteAudioTrack)> {
        self.room
            .remote_participants()
            .into_values()
            .flat_map(|participant| {
                let identity = participant.identity();
                participant.track_publications().into_iter().filter_map(
                    move |(sid, publication)| match publication.track() {
                        Some(RemoteTrack::Audio(track))
                            if publication.encryption_type() == EncryptionType::Gcm =>
                        {
                            Some((identity.clone(), sid, track))
                        }
                        _ => None,
                    },
                )
            })
            .collect()
    }

    /// Publish a microphone. Feed PCM to the source after this call completes.
    pub async fn publish_audio(&self, source: NativeAudioSource) -> Result<LocalTrackPublication> {
        self.publish_audio_track("microphone", source, TrackSource::Microphone)
            .await
    }

    /// Publish one app as a separate encrypted audio track. Feed PCM after this succeeds.
    pub async fn publish_app_audio(
        &self,
        name: &str,
        source: NativeAudioSource,
    ) -> Result<LocalTrackPublication> {
        let key = self
            .name_keys
            .get(&self.epoch)
            .context("missing current media key")?;
        let name = track_name::encrypt(key, track_name::APP_AUDIO, self.epoch, name)?;
        self.publish_audio_track(&name, source, TrackSource::ScreenshareAudio)
            .await
    }

    /// The display name for a subscribed app track, if this is app audio.
    pub fn app_audio_name(&self, track: &RemoteAudioTrack) -> Option<String> {
        if track.source() != TrackSource::ScreenshareAudio {
            return None;
        }
        Some(
            track_name::decrypt(&self.name_keys, track_name::APP_AUDIO, &track.name())
                .unwrap_or_else(|| {
                    tracing::warn!("could not decrypt shared audio track name");
                    "Shared audio".into()
                }),
        )
    }

    async fn publish_audio_track(
        &self,
        name: &str,
        source: NativeAudioSource,
        kind: TrackSource,
    ) -> Result<LocalTrackPublication> {
        let track = livekit::prelude::LocalAudioTrack::create_audio_track(
            name,
            RtcAudioSource::Native(source),
        );
        let publication = self
            .room
            .local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions {
                    source: kind,
                    ..Default::default()
                },
            )
            .await?;
        self.switch_senders();
        Ok(publication)
    }

    /// Stop an audio publication, such as one shared app, while keeping the voice session open.
    pub async fn unpublish_audio(&self, sid: &TrackSid) -> Result<()> {
        self.room.local_participant().unpublish_track(sid).await?;
        Ok(())
    }

    /// Poll WebRTC reports. Track bitrate is available after two samples.
    pub async fn stats(&mut self) -> Result<VoiceStats> {
        self.stats.snapshot(&self.room).await
    }

    /// Exercise the SDK's automatic signaling reconnect (headless e2e).
    pub async fn simulate_signal_reconnect(&self) -> Result<()> {
        self.room
            .simulate_scenario(SimulateScenario::SignalReconnect)
            .await?;
        Ok(())
    }

    /// Disconnect from LiveKit and release media tracks.
    pub async fn leave(mut self) -> Result<()> {
        self.files.disconnect();
        self.room.close().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_key_binds_server_channel_and_epoch() {
        let channel_key = Zeroizing::new([4; 32]);
        let expected = media_key(&channel_key, &[1; 32], &[2; 32], 3);
        assert_eq!(expected, media_key(&channel_key, &[1; 32], &[2; 32], 3));
        assert_ne!(expected, media_key(&channel_key, &[5; 32], &[2; 32], 3));
        assert_ne!(expected, media_key(&channel_key, &[1; 32], &[5; 32], 3));
        assert_ne!(expected, media_key(&channel_key, &[1; 32], &[2; 32], 4));
    }

    #[test]
    fn installing_an_epoch_retires_every_other_slot() {
        let provider = KeyProvider::with_shared_key(
            KeyProviderOptions {
                ratchet_window_size: 0,
                ..Default::default()
            },
            vec![1; 32],
        );
        install_key(&provider, &Zeroizing::new([1; 32]), 1).unwrap();
        install_key(&provider, &Zeroizing::new([2; 32]), 2).unwrap();
        for slot in 0..16 {
            assert_eq!(provider.get_shared_key(slot), Some(vec![2; 32]));
        }
    }

    #[test]
    fn index_wraps_at_16() {
        assert_eq!(key_index(1), 1);
        assert_eq!(key_index(15), 15);
        assert_eq!(key_index(16), 0);
        assert_eq!(key_index(17), 1);
    }
}
