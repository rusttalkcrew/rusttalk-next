mod camera;
mod live_file;
mod screen;

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use reqwest::{Method, StatusCode};
use rusttalk_bot::{
    app_audio,
    voice::{hear_tone, hear_track, send_tone, source},
};
use rusttalk_core::{
    auth::encode,
    client::{
        Event, KeyClient, Server,
        voice::{ParticipantIdentity, RoomEvent, TrackSource, VoiceRoom, VoiceRoster, VoiceStatus},
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

async fn wait_for_microphone(
    room: &mut VoiceRoom,
    who: &ParticipantIdentity,
    muted: bool,
) -> Result<()> {
    timeout(LIMIT, async {
        while let Some(event) = room.next_event().await {
            let (participant, publication, event_muted) = match event {
                RoomEvent::TrackMuted {
                    participant,
                    publication,
                } => (participant, publication, true),
                RoomEvent::TrackUnmuted {
                    participant,
                    publication,
                } => (participant, publication, false),
                _ => continue,
            };
            if participant.identity() == *who
                && publication.source() == TrackSource::Microphone
                && event_muted == muted
            {
                assert_eq!(publication.is_muted(), muted);
                return Ok(());
            }
        }
        anyhow::bail!("room closed while waiting for microphone state");
    })
    .await
    .context("waiting for remote microphone state")?
}

pub async fn exercise(
    project: &str,
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

    // Channels start at 64 kbps; text channels have none; limits hold.
    let catalog = a.catalog().await?;
    let kbps = |id: &[u8; 32]| catalog.channels.iter().find(|c| &c.id == id)?.audio_kbps;
    assert_eq!((kbps(&voice.id), kbps(&text.id)), (Some(64), None));
    let update = |value: i64| {
        a.request(Method::PUT, &format!("/v1/channels/{channel}"))
            .json(
                &json!({ "category_id": encode(&category), "position": 0, "name_ct": "",
                           "audio_kbps": value }),
            )
            .send()
    };
    rejected(
        update(7).await?,
        StatusCode::BAD_REQUEST,
        "invalid_audio_kbps",
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

    live_file::data_only(a, &mut b, &mut c, text, voice).await?;

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
    set_flags(a, &channel, "member", 1).await?;
    a.sync().await?;
    receive_key(&mut b, voice, 1).await?;
    receive_key(&mut c, voice, 1).await?;
    assert_eq!(
        a.voice_participants(&voice.id).await?,
        VoiceRoster::default()
    );

    let (mut room_a, mut room_b) =
        tokio::try_join!(a.join_voice(&voice.id), b.join_voice(&voice.id))?;
    live_file::exercise(project, a, &mut b, &c, &mut room_a, &mut room_b, voice).await?;
    let mut listener = c.join_voice(&voice.id).await?;
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
    senders.spawn(send_tone(b_source.clone(), move || {
        pitch.load(Ordering::Relaxed)
    }));
    let a_id = room_a.identity();
    let flow = async {
        timeout(LIMIT, both_ways(&mut room_a, &mut room_b, A_HZ, B_HZ))
            .await
            .context("initial duplex")??;
        // A new bitrate republishes our audio; the call keeps flowing both ways.
        let replaced = room_a.set_audio_kbps(32).await?;
        assert_eq!(replaced.len(), 1);
        let b_id = room_b.identity();
        timeout(LIMIT, hear_track(&mut room_b, &replaced[0].1, A_HZ))
            .await
            .context("B hearing A after a bitrate change")??;
        timeout(LIMIT, hear_tone(&mut room_a, Some(&b_id), B_HZ))
            .await
            .context("A hearing B after a bitrate change")??;
        for muted in [true, false] {
            room_a.set_microphone_muted(muted);
            wait_for_microphone(&mut room_b, &a_id, muted).await?;
        }
        let people = timeout(LIMIT, async {
            loop {
                let event = a
                    .next_event()
                    .await?
                    .context("missing voice_changed notice")?;
                if matches!(event, Event::VoiceChanged { channel: id } if id == channel) {
                    let roster = a.voice_participants(&voice.id).await?;
                    if roster.people.len() == 3 {
                        return Ok::<_, anyhow::Error>(roster.people);
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
    let listener_id = listener.identity();
    for deafened in [true, false] {
        listener.set_deafened(deafened).await?;
        timeout(LIMIT, async {
            while let Some(event) = room_b.next_event().await {
                if let RoomEvent::ParticipantAttributesChanged { participant, .. } = event
                    && participant.identity() == listener_id
                {
                    let (remote, _) = room_b
                        .participants()
                        .into_iter()
                        .find(|(peer, _)| peer.identity() == listener_id)
                        .context("missing listener")?;
                    assert_eq!(
                        VoiceStatus::from_participant(&remote),
                        VoiceStatus {
                            muted: false,
                            deafened
                        }
                    );
                    return Ok(());
                }
            }
            anyhow::bail!("room closed while waiting for deafen state");
        })
        .await
        .context("waiting for remote deafen state")??;
    }
    println!("Voice microphone mute and listener deafen updates reached another participant");
    println!(
        "W08 encrypted voice both ways, voice_changed and room presence, subscribe-only audio, and automatic signal reconnect"
    );

    ensure!(
        app_audio::available(),
        "W09 app audio needs PipeWire with pipewire-pulse on the host; set E2E_FEATURES= to skip media"
    );
    let capture = app_audio::share(&room_a).await?;
    timeout(LIMIT, app_audio::hear(&mut room_b, Some(&a_id)))
        .await
        .context("waiting for two per-app tracks")??;
    capture.stop(&room_a).await?;

    let listener =
        screen::exercise(&room_a, &mut room_b, listener, server, &mut c, &voice.id).await?;
    let mut listener = camera::exercise(
        &room_a,
        &mut room_b,
        listener,
        a,
        &mut c,
        &channel,
        &voice.id,
    )
    .await?;

    let stats = room_a.stats().await?;
    assert!(
        stats
            .tracks
            .iter()
            .any(|track| !track.outbound && track.packets_lost.is_some())
    );

    // Taking `speak` from B's role must make LiveKit stop forwarding B's audio.
    let b_id = room_b.identity();
    set_flags(a, &channel, "admin", 11).await?;
    // Only a full second without B's tone ends the wait; a closed room or stream keeps
    // waiting until LIMIT fails the test.
    timeout(LIMIT, async {
        while timeout(
            Duration::from_secs(1),
            hear_tone(&mut room_a, Some(&b_id), 780),
        )
        .await
        .is_ok()
        {}
    })
    .await
    .context("B was still audible after losing speak")?;
    set_flags(a, &channel, "admin", 15).await?;
    let _ = room_b.leave().await;
    room_b = b.join_voice(&voice.id).await?;
    room_b.publish_audio(b_source).await?;
    timeout(LIMIT, both_ways(&mut room_a, &mut room_b, A_HZ, 780))
        .await
        .context("duplex after speak returned")??;
    println!("W08 removing speak stopped the speaker's audio, and rejoining restored it");

    // Kick speaker B while its tone keeps playing.
    b_pitch.store(1100, Ordering::Relaxed);
    a.request(
        Method::DELETE,
        &format!("/v1/members/{}", encode(b_device.certificate.person_id())),
    )
    .send()
    .await?
    .error_for_status()?;
    timeout(LIMIT, async {
        while let Some(event) = room_b.next_event().await {
            if let RoomEvent::Disconnected { .. } = event {
                return Ok(());
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("LiveKit did not remove the kicked speaker")??;
    a.sync().await?;
    receive_key(a, voice, 2).await?;
    receive_key(&mut c, voice, 2).await?;
    assert!(room_a.sync_key(a)? && listener.sync_key(&c)?);
    assert_eq!((room_a.epoch(), listener.epoch()), (2, 2));
    // A new tone separates post-rotation media from the jitter buffer's concealed audio.
    a_pitch.store(880, Ordering::Relaxed);
    timeout(LIMIT, hear_tone(&mut listener, Some(&a_id), 880))
        .await
        .context("listener after key rotation")??;
    for room in [&mut room_a, &mut listener] {
        assert!(
            room.participants()
                .iter()
                .all(|(peer, _)| peer.identity() != b_id),
            "kicked speaker still in the room"
        );
    }
    roster(a, &voice.id, 2).await?;
    println!(
        "W08 kick removed the speaking device from the room, rotation kept the listener hearing, and no room still listed the kicked speaker"
    );

    let second = a.create_channel(&category, "voice", b"Second", b"").await?;
    for (from, to) in [(&voice.id, &second.id), (&second.id, &voice.id)] {
        room_a = switch(a, room_a, from, to).await?;
    }
    println!(
        "#147 switching voice channels never listed the device in the channel it left once that was reported"
    );

    senders.abort_all();
    while let Some(result) = senders.join_next().await {
        if let Ok(result) = result {
            result?;
        }
    }
    // LiveKit already closed B's session.
    let _ = room_b.leave().await;
    tokio::try_join!(room_a.leave(), listener.leave())?;
    roster(a, &voice.id, 0).await
}

/// Switch `a` from one voice channel to another the way the app does (leave in the
/// background, join at once). The refetch on the leave notice must not list it in the
/// channel it left, even though that roster was cached with it just before.
async fn switch(
    a: &mut KeyClient<'_>,
    room: VoiceRoom,
    from: &[u8; 32],
    to: &[u8; 32],
) -> Result<VoiceRoom> {
    let me = *a.identity.certificate.person_id();
    ensure!(a.voice_participants(from).await?.people.contains(&me));
    // An older notice would let the check below run before the leave was reported.
    while let Ok(event) = timeout(Duration::from_millis(200), a.next_event()).await {
        event?.context("event stream closed")?;
    }
    let leaving = tokio::spawn(room.leave());
    let room = a.join_voice(to).await?;
    leaving.await??;
    let left = encode(from);
    timeout(LIMIT, async {
        loop {
            let event = a.next_event().await?.context("event stream closed")?;
            if matches!(event, Event::VoiceChanged { channel } if channel == left) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await
    .context("waiting for the leave notice")??;
    let roster = a.voice_participants(from).await?;
    ensure!(
        !roster.people.contains(&me),
        "still listed in the channel it left"
    );
    timeout(LIMIT, async {
        while !a.voice_participants(to).await?.people.contains(&me) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("not listed in the new channel")??;
    Ok(room)
}

async fn set_flags(a: &KeyClient<'_>, channel: &str, role: &str, flags: u8) -> Result<()> {
    a.request(
        Method::PUT,
        &format!("/v1/channels/{channel}/permissions/{role}"),
    )
    .json(&json!({ "flags": flags }))
    .send()
    .await?
    .error_for_status()?;
    Ok(())
}

/// Wait out the API's short roster cache until only `people` known people remain.
async fn roster(a: &KeyClient<'_>, voice: &[u8; 32], people: usize) -> Result<()> {
    timeout(LIMIT, async {
        loop {
            let roster = a.voice_participants(voice).await?;
            if roster.people.len() == people && roster.unknown == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("voice roster did not settle")?
}
