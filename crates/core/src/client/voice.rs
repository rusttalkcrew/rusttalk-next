//! Pinned, end-to-end encrypted voice sessions and caller-owned audio I/O.

mod stats;

use anyhow::{Context, Result, ensure};
use hkdf::Hkdf;
use livekit::{
    Room, RoomOptions,
    e2ee::{
        E2eeOptions, EncryptionType,
        key_provider::{KeyProvider, KeyProviderOptions},
    },
    options::TrackPublishOptions,
    participant::ParticipantState,
    prelude::{LocalTrack, RemoteTrack, TrackSource},
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
    },
    webrtc::{audio_source::native::NativeAudioSource, audio_stream::native::NativeAudioStream},
};

use crate::{
    auth::{KnownServer, decode, encode},
    keys::{Key, Kind, Scope},
    pinned_tls,
};

use super::KeyClient;
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
        let mut url = server.address.clone();
        url.set_scheme("wss")
            .map_err(|_| anyhow::anyhow!("invalid LiveKit signaling origin"))?;
        pinned_tls::register(&url, server.tls_fingerprint).map_err(anyhow::Error::from_boxed)?;
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
        let (room, events) = Room::connect(url.as_str(), &reply.token, options)
            .await
            .context("connect to pinned LiveKit room")?;
        Ok(VoiceRoom {
            room,
            events,
            server: server.clone(),
            channel: *channel,
            epoch,
            provider,
            stats: stats::Sampler::default(),
        })
    }

    /// Fetch unique person IDs currently in a voice channel.
    pub async fn voice_participants(&self, channel: &[u8; 32]) -> Result<Vec<[u8; 32]>> {
        let people: Vec<String> = self
            .request(
                Method::GET,
                &format!("/v1/voice/{}/participants", encode(channel)),
            )
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(people
            .iter()
            .map(|id| decode(id))
            .collect::<Result<_, _>>()?)
    }
}

// SDK senders start at index zero, including auto-republished senders on full reconnect.
// Keep that slot current so such a sender can never emit frames under a retired key.
fn install_key(provider: &KeyProvider, key: &Key, epoch: i64) -> Result<()> {
    let index = key_index(epoch);
    if index != 0 {
        ensure!(
            provider.set_shared_key(key.to_vec(), 0),
            "LiveKit rejected fallback key slot"
        );
    }
    ensure!(
        provider.set_shared_key(key.to_vec(), index),
        "LiveKit rejected media key"
    );
    Ok(())
}

/// A joined voice channel. Use the API events to reconcile keys alongside room events.
pub struct VoiceRoom {
    room: Room,
    events: mpsc::UnboundedReceiver<RoomEvent>,
    server: KnownServer,
    channel: [u8; 32],
    epoch: i64,
    provider: KeyProvider,
    stats: stats::Sampler,
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
        self.room
            .local_participant()
            .permission()
            .is_some_and(|p| p.can_publish)
    }

    /// Install a newer verified channel key after KeyClient reconciles an API event.
    pub fn sync_key(&mut self, keys: &KeyClient<'_>) -> Result<bool> {
        ensure!(
            &self.server == keys.server.known_server(),
            "key client belongs to a different server"
        );
        let (epoch, key) = keys.current_key(scope(&self.channel))?;
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
            match &event {
                RoomEvent::TokenRefreshed { .. } => continue,
                RoomEvent::TrackSubscribed { publication, .. }
                    if publication.encryption_type() != EncryptionType::Gcm =>
                {
                    publication.set_subscribed(false);
                    tracing::warn!("refused unencrypted voice track");
                    continue;
                }
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
        let track = livekit::prelude::LocalAudioTrack::create_audio_track(
            "microphone",
            RtcAudioSource::Native(source),
        );
        let publication = self
            .room
            .local_participant()
            .publish_track(
                LocalTrack::Audio(track),
                TrackPublishOptions {
                    source: TrackSource::Microphone,
                    ..Default::default()
                },
            )
            .await?;
        self.switch_senders();
        Ok(publication)
    }

    /// Stop an audio publication while keeping the voice session open.
    pub async fn unpublish_audio(&self, sid: &TrackSid) -> Result<()> {
        self.room.local_participant().unpublish_track(sid).await?;
        Ok(())
    }

    /// Poll WebRTC reports. Track bitrate is available after two samples.
    pub async fn stats(&mut self) -> Result<VoiceStats> {
        self.stats
            .snapshot(&self.room, self.remote_audio_tracks())
            .await
    }

    /// Exercise the SDK's automatic signaling reconnect (headless e2e).
    pub async fn simulate_signal_reconnect(&self) -> Result<()> {
        self.room
            .simulate_scenario(SimulateScenario::SignalReconnect)
            .await?;
        Ok(())
    }

    /// Disconnect from LiveKit and release media tracks.
    pub async fn leave(self) -> Result<()> {
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
    fn index_wraps_at_16() {
        assert_eq!(key_index(1), 1);
        assert_eq!(key_index(15), 15);
        assert_eq!(key_index(16), 0);
        assert_eq!(key_index(17), 1);
    }
}
