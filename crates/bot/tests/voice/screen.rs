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
        voice::{NativeVideoStream, RoomEvent, TrackSid, TrackSource, VoiceRoom, media_key},
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
    // The share's sound: only people watching the share may receive it.
    let sound = a
        .publish_stream_audio(STREAM_SOUND, rusttalk_core::capture::audio_source())
        .await?
        .sid();
    let result = async {
        let found =
            tokio::time::timeout(LIMIT, screen::find(&mut listener, TrackSource::Screenshare))
                .await??;
        assert_eq!(found.sid, sid);
        assert!(found.track.is_none());
        tokio::time::timeout(LIMIT, screen::hear(b, TrackSource::Screenshare))
            .await
            .context("screen pattern with current key")??;
        tokio::time::timeout(LIMIT, stream_sound(b, true))
            .await
            .context("stream sound arriving with the watched share")??;
        if let Ok(error) = tokio::time::timeout(Duration::from_millis(400), async {
            while let Some(event) = listener.next_event().await {
                if let RoomEvent::TrackSubscribed { track, .. } = event {
                    match track {
                        RemoteTrack::Video(_) => return "an idle viewer received video",
                        RemoteTrack::Audio(audio)
                            if audio.source() == TrackSource::ScreenshareAudio =>
                        {
                            return "an idle viewer received the stream's sound";
                        }
                        RemoteTrack::Audio(_) => {}
                    }
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
        ensure!(
            !(listener.remote_audio_tracks().iter())
                .any(|(_, _, track)| track.source() == TrackSource::ScreenshareAudio),
            "an idle viewer holds the stream's sound"
        );
        listener.leave().await?;
        wrong_key(server, c, channel_id, a, &sid).await?;
        b.set_subscribed(&sid, false)?;
        tokio::time::timeout(LIMIT, stream_sound(b, false))
            .await
            .context("stream sound leaving with the share")??;
        println!(
            "W10 encrypted 320x180 synthetic screen, decrypted name and color bars, no video without subscription or with the wrong key"
        );
        println!("Stream sound received only while watching the share");
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        result = result => result?,
        result = screen::send(source) => result?,
    }
    a.unpublish_audio(&sound).await?;
    a.unpublish_video(&sid).await?;
    c.join_voice(channel_id).await
}

const STREAM_SOUND: &str = "All system audio";

/// Wait until `room` holds (or no longer holds) the share's decrypted sound track.
async fn stream_sound(room: &mut VoiceRoom, held: bool) -> Result<()> {
    loop {
        let found = (room.remote_audio_tracks().iter()).any(|(_, _, track)| {
            room.app_audio_name(track).as_deref() == Some(STREAM_SOUND)
                && track.name() != STREAM_SOUND
        });
        if found == held {
            return Ok(());
        }
        room.next_event()
            .await
            .context("room closed waiting for the stream's sound")?;
    }
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
