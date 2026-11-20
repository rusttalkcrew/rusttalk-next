use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use livekit::webrtc::native::frame_cryptor::EncryptionState;
use reqwest::{Method, StatusCode};
use rusttalk_bot::voice::{hear_tone, send_tone, source};
use rusttalk_core::{
    auth::encode,
    client::{
        Event, KeyClient, Server,
        voice::{RoomEvent, VoiceRoom},
    },
    identity::DeviceIdentity,
};
use serde_json::json;
use tokio::{task::JoinSet, time::timeout};

use super::{invite, receive_key, rejected, session};

const A_HZ: u32 = 440;
const B_HZ: u32 = 660;
const LIMIT: Duration = Duration::from_secs(5);

async fn both_ways(a: &mut VoiceRoom, b: &mut VoiceRoom, a_hz: u32, b_hz: u32) -> Result<()> {
    let (a_id, b_id) = (a.identity(), b.identity());
    tokio::try_join!(
        hear_tone(a, Some(&b_id), b_hz),
        hear_tone(b, Some(&a_id), a_hz),
    )?;
    Ok(())
}

pub async fn exercise(
    server: &Server,
    a: &mut KeyClient<'_>,
    secrets: &mut Vec<String>,
) -> Result<()> {
    let category = a.create_category(b"Voice checks").await?;
    let voice = a.create_channel(&category, "voice", b"Duplex", b"").await?;
    let channel = encode(&voice.id);
    let text = a
        .create_channel(&category, "text", b"Text only", b"")
        .await?;
    rejected(
        a.request(
            Method::POST,
            &format!("/v1/voice/{}/token", encode(&text.id)),
        )
        .send()
        .await?,
        StatusCode::BAD_REQUEST,
        "not_a_voice_channel",
    )
    .await?;

    let (_, b_invite) = invite(a, secrets).await?;
    let (_, b_device) = DeviceIdentity::create("Voice B")?;
    let b_session = session(
        server.sign_in(&b_device, Some(&b_invite.code)).await?,
        secrets,
    )
    .await?;
    let mut b = KeyClient::connect(server, &b_device, &b_session).await?;
    let (_, c_invite) = invite(a, secrets).await?;
    let (_, c_device) = DeviceIdentity::create("Voice listener")?;
    let c_session = session(
        server.sign_in(&c_device, Some(&c_invite.code)).await?,
        secrets,
    )
    .await?;
    let mut c = KeyClient::connect(server, &c_device, &c_session).await?;

    // Only A (owner) and B (admin) may speak. C keeps read access.
    a.request(
        Method::PUT,
        &format!(
            "/v1/members/{}/roles",
            encode(b_device.certificate.person_id())
        ),
    )
    .json(&json!({ "role_ids": ["admin", "member"] }))
    .send()
    .await?
    .error_for_status()?;
    a.request(
        Method::PUT,
        &format!("/v1/channels/{channel}/permissions/member"),
    )
    .json(&json!({ "flags": 1 }))
    .send()
    .await?
    .error_for_status()?;
    a.sync().await?;
    receive_key(&mut b, voice, 1).await?;
    receive_key(&mut c, voice, 1).await?;
    assert!(a.voice_participants(&voice.id).await?.is_empty());

    let (mut room_a, mut room_b, mut listener) = tokio::try_join!(
        a.join_voice(&voice.id),
        b.join_voice(&voice.id),
        c.join_voice(&voice.id),
    )?;
    assert_eq!(
        (
            room_a.can_publish(),
            room_b.can_publish(),
            listener.can_publish()
        ),
        (true, true, false)
    );

    let mut senders = JoinSet::new();
    let a_source = source();
    let b_source = source();
    tokio::try_join!(
        room_a.publish_audio(a_source.clone()),
        room_b.publish_audio(b_source.clone())
    )?;
    let a_pitch = Arc::new(AtomicU32::new(A_HZ));
    let b_pitch = Arc::new(AtomicU32::new(B_HZ));
    let pitch = a_pitch.clone();
    senders.spawn(send_tone(a_source, move || pitch.load(Ordering::Relaxed)));
    let pitch = b_pitch.clone();
    senders.spawn(send_tone(b_source, move || pitch.load(Ordering::Relaxed)));
    let a_id = room_a.identity();
    let flow = async {
        timeout(LIMIT, both_ways(&mut room_a, &mut room_b, A_HZ, B_HZ))
            .await
            .context("initial duplex")??;
        let people = timeout(LIMIT, async {
            loop {
                let event = a
                    .next_event()
                    .await?
                    .context("missing voice_changed notice")?;
                if matches!(event, Event::VoiceChanged { channel: id } if id == channel) {
                    let roster = a.voice_participants(&voice.id).await?;
                    if roster.len() == 3 {
                        return Ok::<_, anyhow::Error>(roster);
                    }
                }
            }
        })
        .await
        .context("waiting for voice presence")??;
        for peer in [a.identity, &b_device, &c_device] {
            assert!(people.contains(peer.certificate.person_id()));
        }

        room_a.simulate_signal_reconnect().await?;
        timeout(LIMIT, async {
            let mut reconnecting = false;
            while let Some(event) = room_a.next_event().await {
                match event {
                    RoomEvent::Reconnecting => reconnecting = true,
                    RoomEvent::Reconnected if reconnecting => return Ok(()),
                    _ => (),
                }
            }
            anyhow::bail!("room closed while reconnecting");
        })
        .await
        .context("waiting for reconnect events")??;
        b_pitch.store(780, Ordering::Relaxed);
        timeout(LIMIT, both_ways(&mut room_a, &mut room_b, A_HZ, 780))
            .await
            .context("duplex after reconnect")??;
        Ok::<_, anyhow::Error>(())
    };
    let refused = async {
        // LiveKit discards a denied AddTrack request. The SDK reports its own publication error.
        let error = timeout(Duration::from_secs(12), listener.publish_audio(source()))
            .await
            .context("waiting for LiveKit publication result")?;
        ensure!(error.is_err(), "LiveKit allowed the listener to publish");
        timeout(LIMIT, hear_tone(&mut listener, Some(&a_id), A_HZ))
            .await
            .context("listener did not receive encrypted audio")??;
        Ok(())
    };
    tokio::try_join!(flow, refused)?;
    println!(
        "W08 encrypted voice both ways, voice_changed and room presence, subscribe-only audio, and automatic signal reconnect"
    );

    a.request(
        Method::DELETE,
        &format!("/v1/members/{}", encode(c_device.certificate.person_id())),
    )
    .send()
    .await?
    .error_for_status()?;
    a.sync().await?;
    receive_key(a, voice, 2).await?;
    receive_key(&mut b, voice, 2).await?;
    assert!(room_a.sync_key(a)? && room_b.sync_key(&b)?);
    assert_eq!((room_a.epoch(), room_b.epoch()), (2, 2));
    // A new tone separates post-rotation media from the jitter buffer's concealed audio.
    a_pitch.store(880, Ordering::Relaxed);
    b_pitch.store(1100, Ordering::Relaxed);
    timeout(LIMIT, both_ways(&mut room_a, &mut room_b, 880, 1100))
        .await
        .context("duplex after key rotation")??;
    timeout(LIMIT, async {
        while let Some(event) = listener.next_event().await {
            if let RoomEvent::E2eeStateChanged {
                participant,
                state: EncryptionState::MissingKey,
            } = event
                && participant.identity() == a_id
            {
                return Ok(());
            }
        }
        anyhow::bail!("listener disconnected before checking encryption")
    })
    .await
    .context("excluded listener did not report a missing media key")??;
    let old_key_result = timeout(
        Duration::from_secs(1),
        hear_tone(&mut listener, Some(&a_id), 880),
    )
    .await;
    assert!(
        old_key_result.is_err(),
        "excluded listener received new audio or lost its track"
    );
    assert_eq!(a.voice_participants(&voice.id).await?.len(), 2);
    println!(
        "W08 key rotation preserved duplex audio and the kicked listener could no longer hear it"
    );

    let stats = room_a.stats().await?;
    assert!(
        stats
            .tracks
            .iter()
            .any(|track| !track.outbound && track.packets_lost.is_some())
    );
    senders.abort_all();
    while let Some(result) = senders.join_next().await {
        if let Ok(result) = result {
            result?;
        }
    }
    tokio::try_join!(room_a.leave(), room_b.leave(), listener.leave())?;
    assert!(a.voice_participants(&voice.id).await?.is_empty());
    Ok(())
}
