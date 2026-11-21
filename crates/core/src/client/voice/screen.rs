use anyhow::{Context, Result, ensure};
use livekit::{
    e2ee::EncryptionType,
    options::{DegradationPreference, TrackPublishOptions, VideoCodec, VideoEncoding},
    prelude::{LocalTrack, LocalVideoTrack, RemoteTrack, TrackKind},
    webrtc::video_source::RtcVideoSource,
};

pub use livekit::{
    prelude::RemoteVideoTrack,
    webrtc::{video_source::native::NativeVideoSource, video_stream::native::NativeVideoStream},
};

use crate::{camera::CameraOptions, screen::ScreenOptions};

use super::{
    LocalTrackPublication, ParticipantIdentity, TrackSid, TrackSource, VoiceRoom,
    stream_audio::is_stream_audio, track_name,
};

/// A remote video publication. The track becomes available after subscription.
pub struct RemoteVideo {
    pub participant: ParticipantIdentity,
    pub sid: TrackSid,
    pub source: TrackSource,
    pub name: String,
    pub track: Option<RemoteVideoTrack>,
}

impl VoiceRoom {
    pub fn can_share_screen(&self) -> bool {
        self.can_publish_source(livekit_protocol::TrackSource::ScreenShare)
    }

    /// Publish a single-layer VP9 screen track. Its label uses the publication epoch.
    pub async fn publish_screen(
        &self,
        name: &str,
        options: ScreenOptions,
        source: NativeVideoSource,
    ) -> Result<LocalTrackPublication> {
        options.validate()?;
        ensure!(
            self.can_share_screen(),
            "screen publication is not permitted"
        );
        let key = self
            .screen_names
            .get(&self.epoch)
            .context("missing current screen name key")?;
        let name = track_name::encrypt(key, track_name::SCREEN, self.epoch, name)?;
        self.publish_video(&name, source, TrackSource::Screenshare, options.fps)
            .await
    }

    pub fn can_use_camera(&self) -> bool {
        self.can_publish_source(livekit_protocol::TrackSource::Camera)
    }

    /// Publish a single-layer VP9 camera track. The track name never names the device.
    /// Fails at once unless the token grants the camera source (see `can_use_camera`).
    pub async fn publish_camera(
        &self,
        options: CameraOptions,
        source: NativeVideoSource,
    ) -> Result<LocalTrackPublication> {
        options.validate()?;
        ensure!(self.can_use_camera(), "camera publication is not permitted");
        self.publish_video("camera", source, TrackSource::Camera, options.fps)
            .await
    }

    async fn publish_video(
        &self,
        name: &str,
        source: NativeVideoSource,
        kind: TrackSource,
        fps: u32,
    ) -> Result<LocalTrackPublication> {
        let resolution = source.video_resolution();
        let max_bitrate =
            (u64::from(resolution.width) * u64::from(resolution.height) * u64::from(fps) / 12)
                .clamp(300_000, 12_000_000);
        // A 60 fps share is chosen for motion: lose detail before smoothness. Below that,
        // LiveKit keeps a screen's resolution (readable text) and a camera's frame rate.
        let degradation_preference = (kind == TrackSource::Screenshare && fps >= 60)
            .then_some(DegradationPreference::MaintainFramerate);
        let track = LocalVideoTrack::create_video_track(name, RtcVideoSource::Native(source));
        let publication = self
            .room
            .local_participant()
            .publish_track(
                LocalTrack::Video(track),
                TrackPublishOptions {
                    source: kind,
                    // Measured with the SDK's libwebrtc: the top VP8 simulcast or VP9 SVC
                    // layer encodes at 1-4 fps, while one VP9 layer holds 1080p60. Every
                    // client uses the same SDK, so every viewer can decode VP9.
                    video_codec: VideoCodec::VP9,
                    video_encoding: Some(VideoEncoding {
                        max_bitrate,
                        max_framerate: fps.into(),
                    }),
                    simulcast: false,
                    degradation_preference,
                    ..Default::default()
                },
            )
            .await?;
        self.switch_senders();
        Ok(publication)
    }

    /// Stop a screen or camera publication.
    pub async fn unpublish_video(&self, sid: &TrackSid) -> Result<()> {
        self.room.local_participant().unpublish_track(sid).await?;
        Ok(())
    }

    /// Discover encrypted video publications without subscribing.
    pub fn remote_video_tracks(&self) -> Vec<RemoteVideo> {
        self.room
            .remote_participants()
            .into_values()
            .flat_map(|participant| {
                let identity = participant.identity();
                participant
                    .track_publications()
                    .into_values()
                    .filter_map(move |publication| {
                        if publication.kind() != TrackKind::Video
                            || publication.encryption_type() != EncryptionType::Gcm
                        {
                            return None;
                        }
                        let source = publication.source();
                        let name = match source {
                            TrackSource::Screenshare => track_name::decrypt(
                                &self.screen_names,
                                track_name::SCREEN,
                                &publication.name(),
                            )
                            .unwrap_or_else(|| "Shared screen".into()),
                            TrackSource::Camera => "Camera".into(),
                            _ => "Shared video".into(),
                        };
                        let track = match publication.track() {
                            Some(RemoteTrack::Video(track)) => Some(track),
                            _ => None,
                        };
                        Some(RemoteVideo {
                            participant: identity.clone(),
                            sid: publication.sid(),
                            source,
                            name,
                            track,
                        })
                    })
            })
            .collect()
    }

    /// Start or stop receiving one remote video publication. A screen share's sound
    /// comes and goes with it, so only its viewers receive it.
    pub fn set_subscribed(&self, sid: &TrackSid, subscribed: bool) -> Result<()> {
        let (participant, publication) = self
            .room
            .remote_participants()
            .into_values()
            .find_map(|participant| {
                let publication = participant.track_publications().remove(sid)?;
                Some((participant, publication))
            })
            .context("video publication is no longer available")?;
        ensure!(
            publication.kind() == TrackKind::Video
                && publication.encryption_type() == EncryptionType::Gcm,
            "expected an encrypted video publication"
        );
        if publication.is_desired() != subscribed {
            publication.set_subscribed(subscribed);
        }
        if publication.source() == TrackSource::Screenshare {
            for audio in participant.track_publications().into_values() {
                if is_stream_audio(&audio, &self.stream_names) && audio.is_desired() != subscribed {
                    audio.set_subscribed(subscribed);
                }
            }
        }
        Ok(())
    }
}
