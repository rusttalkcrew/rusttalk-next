use std::time::Duration;

use anyhow::{Context, Result, ensure};
use futures_util::StreamExt;
use livekit::{
    Room, RoomOptions,
    e2ee::{
        E2eeOptions, EncryptionType,
        key_provider::{KeyProvider, KeyProviderOptions},
    },
    prelude::RemoteTrack,
};
use reqwest::Method;
use rusttalk_bot::screen;
use rusttalk_core::{
    client::{
        KeyClient, Server,
        voice::{NativeVideoStream, RoomEvent, TrackSid, VoiceRoom, media_key},
    },
    keys::{Kind, Scope},
};

use super::LIMIT;

pub(super) async fn exercise(
    a: &VoiceRoom,
    b: &mut VoiceRoom,
    mut listener: VoiceRoom,
    server: &Server,
    c: &mut KeyClient<'_>,
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
        listener.leave().await?;
        wrong_key(server, c, channel_id, a, &sid).await?;
        b.set_subscribed(&sid, false)?;
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
    channel: &[u8; 32],
    sender: &VoiceRoom,
    sid: &TrackSid,
) -> Result<()> {
    let encoded = rusttalk_core::auth::encode(channel);
    let reply: serde_json::Value = c
        .request(Method::POST, &format!("/v1/voice/{encoded}/token"))
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
    let provider = KeyProvider::with_shared_key(
        KeyProviderOptions {
            ratchet_window_size: 0,
            ..Default::default()
        },
        key.clone(),
    );
    for slot in 0..16 {
        assert!(provider.set_shared_key(key.clone(), slot));
    }
    let mut options = RoomOptions::default();
    options.auto_subscribe = false;
    options.encryption = Some(E2eeOptions {
        encryption_type: EncryptionType::Gcm,
        key_provider: provider.clone(),
    });
    let (room, mut events) = Room::connect(url.as_str(), token, options).await?;
    let result = async {
        let peer = room
            .remote_participants()
            .remove(&sender.identity())
            .context("missing screen sender")?;
        let publication = peer
            .track_publications()
            .remove(sid)
            .context("missing screen publication")?;
        publication.set_subscribed(true);
        let mut video = tokio::time::timeout(LIMIT, async {
            while let Some(event) = events.recv().await {
                if let RoomEvent::TrackSubscribed {
                    track: RemoteTrack::Video(track),
                    ..
                } = event
                    && &track.sid() == sid
                {
                    return Ok(NativeVideoStream::new(track.rtc_track()));
                }
            }
            anyhow::bail!("event stream closed")
        })
        .await
        .context("wrong-key viewer did not subscribe")??;
        ensure!(
            tokio::time::timeout(Duration::from_secs(1), video.next())
                .await
                .is_err(),
            "wrong-key viewer decoded video or its stream closed"
        );
        let (epoch, channel_key) = c.current_key(Scope {
            kind: Kind::Channel,
            id: *channel,
        })?;
        assert_eq!(epoch, sender.epoch());
        let key = media_key(
            channel_key,
            &server.known_server().server_id,
            channel,
            epoch,
        );
        for slot in 0..16 {
            assert!(provider.set_shared_key(key.to_vec(), slot));
        }
        tokio::time::timeout(LIMIT, screen::verify(&mut video))
            .await
            .context("screen pattern after installing the correct key")??;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let closed = room.close().await;
    result?;
    closed?;
    Ok(())
}
