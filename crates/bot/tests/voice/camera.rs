use std::time::Duration;

use anyhow::{Context, Result, ensure};
use rusttalk_bot::screen;
use rusttalk_core::client::{
    KeyClient,
    voice::{TrackSource, VoiceRoom},
};

use super::{LIMIT, set_flags};

/// A publishes a synthetic camera that B decodes. A member who may speak but not share
/// video gets a token that LiveKit holds to the microphone.
/// C's listener session is replaced, since one device holds one room identity.
pub(super) async fn exercise(
    a: &VoiceRoom,
    b: &mut VoiceRoom,
    listener: VoiceRoom,
    owner: &KeyClient<'_>,
    c: &mut KeyClient<'_>,
    channel: &str,
    channel_id: &[u8; 32],
) -> Result<VoiceRoom> {
    assert!(a.can_use_camera());
    let source = screen::source();
    let sid = a
        .publish_camera(screen::camera_options(), source.clone())
        .await?
        .sid();
    let result = async {
        let camera = tokio::time::timeout(LIMIT, screen::find(b, TrackSource::Camera)).await??;
        assert_eq!((camera.sid, camera.name.as_str()), (sid.clone(), "Camera"));
        tokio::time::timeout(LIMIT, screen::hear(b, TrackSource::Camera))
            .await
            .context("camera pattern")??;
        b.set_subscribed(&sid, false)?;

        listener.leave().await?;
        set_flags(owner, channel, "member", 5).await?;
        let speaker = c.join_voice(channel_id).await?;
        let refused = async {
            ensure!(speaker.can_publish() && !speaker.can_use_camera());
            // LiveKit discards a denied AddTrack request. The SDK reports its own error.
            let published = tokio::time::timeout(
                Duration::from_secs(12),
                speaker.publish_camera(screen::camera_options(), screen::source()),
            )
            .await
            .context("waiting for LiveKit camera publication result")?;
            ensure!(published.is_err(), "LiveKit allowed a camera without video");
            Ok(())
        }
        .await;
        speaker.leave().await?;
        set_flags(owner, channel, "member", 1).await?;
        refused?;
        println!(
            "W11 encrypted simulcast camera decoded by a viewer, and refused for a member without video"
        );
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        result = result => result?,
        result = screen::send(source) => result?,
    }
    a.unpublish_video(&sid).await?;
    c.join_voice(channel_id).await
}
