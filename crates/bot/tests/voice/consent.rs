//! Synthetic media never enters a listener's decoder before publication-specific consent.
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use livekit::prelude::RemoteTrack;
use rusttalk_bot::voice::{hear_tone, hear_track, send_tone, source};
use rusttalk_core::client::voice::{RoomEvent, TrackSid, TrackSource, VoiceRoom};
use tokio::time::{Instant, timeout, timeout_at};

pub(super) async fn listen(room: &mut VoiceRoom, sid: &TrackSid) -> Result<()> {
    timeout(super::hang_guard(), async {
        while !room.shared_audio().iter().any(|(_, id, _)| id == sid) {
            room.next_event()
                .await
                .context("room closed before audio offer")?;
        }
        room.set_listening(sid, true)
    })
    .await?
}

async fn silent(room: &mut VoiceRoom) -> Result<()> {
    let until = Instant::now() + Duration::from_millis(700);
    while let Ok(event) = timeout_at(until, room.next_event()).await {
        if let Some(RoomEvent::TrackSubscribed {
            track: RemoteTrack::Audio(track),
            ..
        }) = event
        {
            ensure!(
                track.source() != TrackSource::ScreenshareAudio,
                "shared audio reached playback before consent"
            );
        }
    }
    // Inspect SDK publications too: filtering our snapshot alone could hide a leak.
    for (p, _) in room.participants() {
        for publication in p.track_publications().into_values() {
            if publication.source() == TrackSource::ScreenshareAudio {
                ensure!(
                    !publication.is_desired() && !publication.is_subscribed(),
                    "unconsented shared audio subscription"
                );
            }
        }
    }
    Ok(())
}

pub(super) async fn exercise(
    sender: &VoiceRoom,
    listener: &mut VoiceRoom,
    mic_hz: u32,
    newcomer: &mut rusttalk_core::client::KeyClient<'_>,
    channel: &[u8; 32],
) -> Result<()> {
    let source = source();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(send_tone(source.clone(), || 990));
    let first = sender
        .publish_app_audio("Consent tone", source.clone())
        .await?
        .sid();
    silent(listener).await?;
    sender.announce_phone(Some(&first), &[]).await?;
    let call = sender.local_phone_call(&first);
    wait_phone(|| listener.external_phone_calls().contains(&call)).await?;
    ensure!(
        sender.phone_microphones(&call).is_empty(),
        "phone exports before consent"
    );
    // Listen is deliberately insufficient to export anyone's voice.
    listen(listener, &first).await?;
    ensure!(
        sender.phone_microphones(&call).is_empty(),
        "Listen granted phone export"
    );
    listener
        .announce_phone(None, std::slice::from_ref(&call))
        .await?;
    wait_phone(|| !sender.phone_microphones(&call).is_empty()).await?;
    listener.announce_phone(None, &[]).await?;
    wait_phone(|| sender.phone_microphones(&call).is_empty()).await?;
    listener.set_listening(&first, false)?;
    sender.announce_phone(None, &[]).await?;
    wait_phone(|| listener.external_phone_calls().is_empty()).await?;
    println!(
        "Phone consent: default silence; Listen does not grant export; explicit permission and withdrawal bound to current call"
    );
    let mut late = newcomer.join_voice(channel).await?;
    silent(&mut late).await?;
    timeout(
        super::hang_guard(),
        hear_tone(&mut late, Some(&sender.identity()), mic_hz),
    )
    .await??;
    late.leave().await?;
    timeout(
        super::hang_guard(),
        hear_tone(listener, Some(&sender.identity()), mic_hz),
    )
    .await??;
    listen(listener, &first).await?;
    timeout(super::hang_guard(), hear_track(listener, &first, 990)).await??;
    listener.set_listening(&first, false)?;
    silent(listener).await?;
    listen(listener, &first).await?;
    timeout(super::hang_guard(), hear_track(listener, &first, 990)).await??;
    listener.simulate_signal_reconnect().await?;
    timeout(super::hang_guard(), async {
        while let Some(event) = listener.next_event().await {
            if matches!(event, RoomEvent::Reconnected) {
                return Ok::<_, anyhow::Error>(());
            }
        }
        anyhow::bail!("room closed during consent reconnect test");
    })
    .await??;
    silent(listener).await?;
    timeout(
        super::hang_guard(),
        hear_tone(listener, Some(&sender.identity()), mic_hz),
    )
    .await??;
    listen(listener, &first).await?;
    timeout(super::hang_guard(), hear_track(listener, &first, 990)).await??;
    sender.unpublish_audio(&first).await?;
    let replacement = sender
        .publish_app_audio("Consent tone", source)
        .await?
        .sid();
    silent(listener).await?;
    listen(listener, &replacement).await?;
    timeout(super::hang_guard(), hear_track(listener, &replacement, 990)).await??;
    listener.set_listening(&replacement, false)?;
    silent(listener).await?;
    sender.unpublish_audio(&replacement).await?;
    println!(
        "Shared audio: no subscription before consent; decoded 990 Hz after Listen; Stop and replacement silent; microphone uninterrupted"
    );
    Ok(())
}

async fn wait_phone(ready: impl Fn() -> bool) -> Result<()> {
    timeout(super::hang_guard(), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("waiting for encrypted phone consent")?;
    Ok(())
}
