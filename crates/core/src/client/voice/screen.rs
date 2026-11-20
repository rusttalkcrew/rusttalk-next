use anyhow::{Context, Result, ensure};
use livekit::{
    e2ee::EncryptionType,
    options::{TrackPublishOptions, VideoCodec, VideoEncoding},
    prelude::{LocalTrack, LocalVideoTrack, RemoteTrack, TrackKind},
    webrtc::video_source::RtcVideoSource,
};

pub use livekit::{
    prelude::RemoteVideoTrack,
    webrtc::{video_source::native::NativeVideoSource, video_stream::native::NativeVideoStream},
};

use crate::screen::ScreenOptions;

use super::{
    LocalTrackPublication, ParticipantIdentity, TrackSid, TrackSource, VoiceRoom, track_name,
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

    /// Publish a VP8 simulcast screen track. Its label uses the publication epoch.
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
        let resolution = source.video_resolution();
        let max_bitrate =
            (u64::from(resolution.width) * u64::from(resolution.height) * u64::from(options.fps)
                / 12)
                .clamp(300_000, 12_000_000);
        let track = LocalVideoTrack::create_video_track(&name, RtcVideoSource::Native(source));
        let publication = self
            .room
            .local_participant()
            .publish_track(
                LocalTrack::Video(track),
                TrackPublishOptions {
                    source: TrackSource::Screenshare,
                    video_codec: VideoCodec::VP8,
                    video_encoding: Some(VideoEncoding {
                        max_bitrate,
                        max_framerate: options.fps.into(),
                    }),
                    simulcast: true,
                    ..Default::default()
                },
            )
            .await?;
        self.switch_senders();
        Ok(publication)
    }

    pub async fn unpublish_screen(&self, sid: &TrackSid) -> Result<()> {
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

    /// Start or stop receiving one remote video publication.
    pub fn set_subscribed(&self, sid: &TrackSid, subscribed: bool) -> Result<()> {
        let publication = self
            .room
            .remote_participants()
            .into_values()
            .find_map(|participant| participant.track_publications().remove(sid))
            .context("video publication is no longer available")?;
        ensure!(
            publication.kind() == TrackKind::Video
                && publication.encryption_type() == EncryptionType::Gcm,
            "expected an encrypted video publication"
        );
        if publication.is_desired() != subscribed {
            publication.set_subscribed(subscribed);
        }
        Ok(())
    }
}
