use std::time::Duration;

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use livekit::{
    Room, RoomOptions,
    e2ee::{E2eeOptions, EncryptionType, key_provider::KeyProvider},
    prelude::RemoteTrack,
    webrtc::native::frame_cryptor::EncryptionState,
};
use reqwest::Method;
use rusttalk_bot::screen;
use rusttalk_core::client::{
    KeyClient, Server,
    voice::{NativeVideoStream, RoomEvent, VoiceRoom},
};

use super::LIMIT;

pub(super) async fn exercise(
    a: &VoiceRoom,
    b: &mut VoiceRoom,
    mut listener: VoiceRoom,
    server: &Server,
    c: &mut KeyClient<'_>,
    channel: &str,
    channel_id: &[u8; 32],
) -> Result<VoiceRoom> {
    assert!(a.can_share_screen() && !listener.can_share_screen());
    let source = screen::source();
    let sid = a
        .publish_screen(screen::NAME, screen::options(), source.clone())
        .await?
        .sid();
    let result = async {
        let found = tokio::time::timeout(LIMIT, screen::find(&mut listener)).await??;
        assert_eq!(found.sid, sid);
        assert!(found.track.is_none());
        tokio::time::timeout(LIMIT, screen::hear(b))
            .await
            .context("screen pattern with current key")??;
        if let Ok(error) = tokio::time::timeout(Duration::from_millis(400), async {
            while let Some(event) = listener.next_event().await {
                if matches!(
                    event,
                    RoomEvent::TrackSubscribed {
                        track: RemoteTrack::Video(_),
                        ..
                    }
                ) {
                    return "an idle viewer received video";
                }
            }
            "idle viewer disconnected"
        })
        .await
        {
            anyhow::bail!("{error}");
        }
        assert!(
            listener
                .remote_video_tracks()
                .iter()
                .all(|video| video.track.is_none())
        );
        b.set_subscribed(&sid, false)?;
        listener.leave().await?;
        wrong_key(server, c, channel, a, &sid).await?;
        println!(
            "W10 encrypted 320x180 synthetic screen, decrypted name and color bars, no video without subscription or with the wrong key"
        );
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        result = result => result?,
        result = screen::send(source) => result?,
    }
    a.unpublish_screen(&sid).await?;
    c.join_voice(channel_id).await
}

async fn wrong_key(
    server: &Server,
    c: &KeyClient<'_>,
    channel: &str,
    sender: &VoiceRoom,
    sid: &rusttalk_core::client::voice::TrackSid,
) -> Result<()> {
    let reply: serde_json::Value = c
        .request(Method::POST, &format!("/v1/voice/{channel}/token"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let token = reply["token"].as_str().context("missing listener token")?;
    let mut url = server.known_server().address.clone();
    url.set_scheme("wss")
        .map_err(|_| anyhow::anyhow!("invalid signaling origin"))?;
    // This independent receiver can subscribe with its valid token but lacks the room key.
    let key = vec![42; 32];
    let provider = KeyProvider::with_shared_key(Default::default(), key.clone());
    assert!(provider.set_shared_key(key, rusttalk_core::client::voice::key_index(sender.epoch())));
    let mut options = RoomOptions::default();
    options.auto_subscribe = false;
    options.encryption = Some(E2eeOptions {
        encryption_type: EncryptionType::Gcm,
        key_provider: provider,
    });
    let (room, mut events) = Room::connect(url.as_str(), token, options).await?;
    let result = tokio::time::timeout(LIMIT, async {
        let peer = room.remote_participants().remove(&sender.identity()).context("missing screen sender")?;
        let publication = peer.track_publications().remove(sid).context("missing screen publication")?;
        publication.set_subscribed(true);
        let mut video = loop {
            match events.recv().await.context("missing video subscription")? {
                RoomEvent::TrackSubscribed { track: RemoteTrack::Video(track), .. } if &track.sid() == sid => {
                    break NativeVideoStream::new(track.rtc_track());
                }
                _ => (),
            }
        };
        tokio::select! {
            _ = video.next() => anyhow::bail!("wrong-key viewer decoded video or its stream closed"),
            result = async {
                while let Some(event) = events.recv().await {
                    if matches!(event, RoomEvent::E2eeStateChanged { participant, state: EncryptionState::DecryptionFailed, .. }
                        if participant.identity() == sender.identity())
                    {
                        return Ok(());
                    }
                }
                anyhow::bail!("no frame decryption failure reported")
            } => result?,
        }
        ensure!(tokio::time::timeout(Duration::from_millis(400), video.next()).await.is_err(),
            "wrong-key viewer decoded video or its stream closed");
        Ok::<_, anyhow::Error>(())
    }).await.context("wrong-key screen subscription");
    let closed = room.close().await;
    result??;
    closed?;
    Ok(())
}
