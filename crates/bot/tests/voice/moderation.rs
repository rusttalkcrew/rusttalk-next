//! Owner A mutes, removes and moves admin B in voice; member C may not.

use anyhow::{Context, Result, ensure};
use livekit::prelude::Participant;
use reqwest::{Method, StatusCode};
use rusttalk_bot::voice::{hear_track, send_tone, source};
use rusttalk_core::{
    auth::encode,
    client::{
        Event, KeyClient,
        voice::{DisconnectReason, NativeAudioSource, RoomEvent, VoiceRoom},
    },
    keys::Scope,
};
use serde_json::{Value, json};
use tokio::time::timeout;

use super::{A_HZ, both_ways, falls_silent, hang_guard};
use crate::{receive_key, rejected};

const B_HZ: u32 = 780;

#[allow(clippy::too_many_arguments)]
pub async fn exercise(
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
    room_a: &mut VoiceRoom,
    mut room_b: VoiceRoom,
    b_source: NativeAudioSource,
    category: &[u8; 32],
    voice: Scope,
) -> Result<VoiceRoom> {
    let channel = encode(&voice.id);
    let b_person = encode(b.identity.certificate.person_id());
    let mute = format!("/v1/members/{b_person}/voice-mute");
    rejected(
        c.request(Method::PUT, &mute).send().await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;

    // A server mute silences B's microphone in LiveKit, and holds when B rejoins.
    let b_id = room_b.identity();
    const APP_HZ: u32 = 550;
    let app_source = source();
    let mut tones = tokio::task::JoinSet::new();
    tones.spawn(send_tone(app_source.clone(), || APP_HZ));
    let app = room_b.publish_app_audio("Game", app_source.clone()).await?;
    super::consent::listen(room_a, &app.sid()).await?;
    timeout(hang_guard(), hear_track(room_a, &app.sid(), APP_HZ)).await??;
    let stream = room_b.publish_stream_audio("Stream", source()).await?;
    a.request(Method::PUT, &mute)
        .send()
        .await?
        .error_for_status()?;
    falls_silent(room_a, &b_id, B_HZ)
        .await
        .context("B was still audible after a server mute")?;
    let members: Value = a
        .request(Method::GET, "/v1/members")
        .send()
        .await?
        .json()
        .await?;
    let flagged: Vec<_> = (members.as_array().context("member list")?.iter())
        .filter(|member| member["voice_muted"] == true)
        .map(|member| member["person_id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(flagged, [b_person.as_str()]);
    // Keep the same local SDK participant for both transitions.
    sync_audio_grant(&mut room_b, &b_source, false).await?;
    ensure!(
        app.track().is_none() && stream.track().is_none(),
        "shared audio publications survived the deny"
    );
    a.request(Method::DELETE, &mute)
        .send()
        .await?
        .error_for_status()?;
    sync_audio_grant(&mut room_b, &b_source, true).await?;
    timeout(hang_guard(), both_ways(room_a, &mut room_b, A_HZ, B_HZ))
        .await
        .context("duplex after lifting a mute without rejoining")??;

    let again = room_b.publish_app_audio("Game", app_source).await?;
    super::consent::listen(room_a, &again.sid()).await?;
    timeout(hang_guard(), hear_track(room_a, &again.sid(), APP_HZ))
        .await
        .context("restarted game audio after unmute")??;
    room_b.unpublish_audio(&again.sid()).await?;
    drop(tones);

    a.request(Method::PUT, &mute)
        .send()
        .await?
        .error_for_status()?;
    falls_silent(room_a, &b_id, B_HZ).await?;
    let _ = room_b.leave().await;
    room_b = b.join_voice(&voice.id).await?;
    ensure!(
        !room_b.can_publish(),
        "a muted rejoin may publish a microphone"
    );
    a.request(Method::DELETE, &mute)
        .send()
        .await?
        .error_for_status()?;
    // Lifting it gives the microphone back in the room B is in, with no rejoin and without
    // B's API connection: LiveKit's own permission change is enough, as the app relies on.
    sync_audio_grant(&mut room_b, &b_source, true).await?;
    timeout(hang_guard(), both_ways(room_a, &mut room_b, A_HZ, B_HZ))
        .await
        .context("duplex after the mute was lifted")??;
    println!(
        "Server mute silenced B in LiveKit and held across a rejoin; lifting it gave the microphone back in place"
    );

    // Removing disconnects B with LiveKit's removal reason.
    let participant = format!("/v1/voice/{channel}/participants/{b_person}");
    rejected(
        c.request(Method::DELETE, &participant).send().await?,
        StatusCode::FORBIDDEN,
        "permission_denied",
    )
    .await?;
    a.request(Method::DELETE, &participant)
        .send()
        .await?
        .error_for_status()?;
    let reason = disconnected(&mut room_b).await?;
    assert_eq!(reason, Some(DisconnectReason::ParticipantRemoved));
    rejected(
        a.request(Method::DELETE, &participant).send().await?,
        StatusCode::NOT_FOUND,
        "not_in_voice",
    )
    .await?;
    room_b = rejoin(b, room_a, voice, &b_source).await?;

    // Moving tells only B where to go, then disconnects B from this room.
    let lobby = a.create_channel(category, "voice", b"Lobby", b"").await?;
    a.request(Method::POST, &format!("{participant}/move"))
        .json(&json!({ "to": encode(&lobby.id) }))
        .send()
        .await?
        .error_for_status()?;
    disconnected(&mut room_b).await?;
    let b_device = encode(b.identity.certificate.signing_key().as_bytes());
    timeout(hang_guard(), async {
        loop {
            let event = b.next_event().await?.context("socket closed")?;
            if let Event::VoiceMoved { devices, from, to } = event {
                assert_eq!(devices, std::slice::from_ref(&b_device));
                assert_eq!((from, to), (channel.clone(), encode(&lobby.id)));
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await
    .context("waiting for voice_moved")??;
    no_move_notice(a).await?;
    no_move_notice(c).await?;
    // B follows the notice into the lobby, as the app does.
    a.sync().await?;
    receive_key(b, lobby, 1).await?;
    let lobby_room = b.join_voice(&lobby.id).await?;
    let me = *b.identity.certificate.person_id();
    timeout(hang_guard(), async {
        while !a.voice_participants(&lobby.id).await?.people.contains(&me) {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("B did not show up in the lobby")??;
    lobby_room.leave().await?;
    let room_b = rejoin(b, room_a, voice, &b_source).await?;
    let audited: Value = a
        .request(Method::GET, "/v1/audit?action=voice_moved")
        .send()
        .await?
        .json()
        .await?;
    ensure!(
        audited.as_array().is_some_and(|entries| entries.len() == 1),
        "voice_moved audit entry"
    );
    println!("Voice remove and move disconnected B from the room, and only B learned the target");
    Ok(room_b)
}

async fn sync_audio_grant(
    room: &mut VoiceRoom,
    source: &NativeAudioSource,
    allowed: bool,
) -> Result<()> {
    timeout(hang_guard(), async {
        loop {
            if let RoomEvent::ParticipantPermissionChanged {
                participant: Participant::Local(_),
                permission,
            } = room.next_event().await.context("room closed")?
            {
                let denied = room.stop_denied_shared_audio(permission.as_ref()).await;
                room.sync_microphone(permission.as_ref(), source.clone(), || false)
                    .await?;
                if denied != allowed
                    && room.can_publish() == allowed
                    && room.has_microphone() == allowed
                {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        }
    })
    .await
    .context("waiting for microphone publication to follow permission")?
}

/// Nobody but the moved device hears about a move.
async fn no_move_notice(client: &mut KeyClient<'_>) -> Result<()> {
    let seen = timeout(std::time::Duration::from_secs(1), async {
        loop {
            match client.next_event().await {
                Ok(Some(Event::VoiceMoved { .. })) => return true,
                Ok(Some(_)) => {}
                _ => return false,
            }
        }
    })
    .await;
    ensure!(seen != Ok(true), "another member received the move notice");
    Ok(())
}

async fn rejoin(
    b: &mut KeyClient<'_>,
    room_a: &mut VoiceRoom,
    voice: Scope,
    source: &NativeAudioSource,
) -> Result<VoiceRoom> {
    let mut room_b = b.join_voice(&voice.id).await?;
    room_b.publish_audio(source.clone()).await?;
    timeout(hang_guard(), both_ways(room_a, &mut room_b, A_HZ, B_HZ))
        .await
        .context("duplex after rejoining")??;
    Ok(room_b)
}

async fn disconnected(room: &mut VoiceRoom) -> Result<Option<DisconnectReason>> {
    timeout(hang_guard(), async {
        while let Some(event) = room.next_event().await {
            if let RoomEvent::Disconnected { reason } = event {
                return Some(reason);
            }
        }
        None
    })
    .await
    .context("LiveKit did not disconnect B")
}
