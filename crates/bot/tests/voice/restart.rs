//! Disposable service restarts with synthetic encrypted media only.
use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use rusttalk_bot::{
    screen,
    voice::{send_tone, source},
};
use rusttalk_core::client::{
    KeyClient,
    voice::{TrackSource, VoiceRoom},
};
use tokio::{task::JoinSet, time::timeout};

use super::{A_HZ, B_HZ, both_ways, hang_guard, roster};

async fn fresh_room(client: &mut KeyClient<'_>, channel: &[u8; 32]) -> Result<VoiceRoom> {
    timeout(Duration::from_secs(60), async {
        loop {
            match async {
                Box::pin(client.reconnect()).await?;
                Box::pin(client.join_voice(channel)).await
            }
            .await
            {
                Ok(room) => return Ok(room),
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    })
    .await
    .context("fresh authorization after restart timed out")?
}

pub(super) async fn exercise(
    project: &str,
    a: &mut KeyClient<'_>,
    b: &mut KeyClient<'_>,
    c: &mut KeyClient<'_>,
) -> Result<()> {
    ensure!(
        project.starts_with("rusttalk-e2e-"),
        "restart test requires disposable e2e project"
    );
    let category = a.create_category(b"Restart checks").await?;
    let scope = a
        .create_channel(&category, "voice", b"Restart", b"")
        .await?;
    a.sync().await?;
    super::super::receive_key(b, scope, 1).await?;
    super::super::receive_key(c, scope, 1).await?;
    for services in [vec!["api"], vec!["livekit"], vec!["api", "livekit"]] {
        // Keep the three SDK join futures off the test thread's stack; signaling
        // compression also needs stack space (as in the moderation scenario).
        let (mut left, mut right, mut cancelled) = tokio::try_join!(
            Box::pin(a.join_voice(&scope.id)),
            Box::pin(b.join_voice(&scope.id)),
            Box::pin(c.join_voice(&scope.id))
        )?;
        let (mic_a, mic_b, video) = (source(), source(), screen::source());
        let sound = source();
        tokio::try_join!(
            left.publish_audio(mic_a.clone()),
            right.publish_audio(mic_b.clone())
        )?;
        let old_screen = left
            .publish_screen(screen::NAME, screen::options(), video.clone())
            .await?
            .sid();
        let old_sound = left
            .publish_stream_audio("Synthetic shared audio", sound.clone())
            .await?
            .sid();
        let mut senders = JoinSet::new();
        senders.spawn(send_tone(sound.clone(), || 1320));
        let pitch_a = Arc::new(AtomicU32::new(A_HZ));
        let pitch_b = Arc::new(AtomicU32::new(B_HZ));
        let pitch = pitch_a.clone();
        senders.spawn(send_tone(mic_a.clone(), move || {
            pitch.load(Ordering::Relaxed)
        }));
        let pitch = pitch_b.clone();
        senders.spawn(send_tone(mic_b.clone(), move || {
            pitch.load(Ordering::Relaxed)
        }));
        senders.spawn(screen::send(video.clone()));
        timeout(hang_guard(), both_ways(&mut left, &mut right, A_HZ, B_HZ)).await??;
        timeout(
            hang_guard(),
            screen::hear(&mut right, TrackSource::Screenshare),
        )
        .await??;
        right
            .set_video_watches(std::slice::from_ref(&old_screen))
            .await?;
        super::consent::listen(&mut right, &old_sound).await?;
        timeout(
            hang_guard(),
            rusttalk_bot::voice::hear_track(&mut right, &old_sound, 1320),
        )
        .await??;
        roster(a, &scope.id, 3).await?;
        let owner = left.identity();
        let old_session = right
            .participants()
            .into_iter()
            .find(|(peer, _)| peer.identity() == owner)
            .context("screen owner absent before restart")?
            .0
            .sid();

        let name = project.to_owned();
        let stopped = services.clone();
        tokio::task::spawn_blocking(move || {
            let mut args = vec!["stop", "--timeout", "5"];
            args.extend(stopped);
            super::super::compose(&name, &args)
        })
        .await??;
        // Explicit hangup during the outage: never join this third client again.
        let _ = timeout(Duration::from_secs(15), cancelled.disconnect()).await;
        drop(cancelled);
        let name = project.to_owned();
        let started = services.clone();
        tokio::task::spawn_blocking(move || {
            let mut args = vec!["start"];
            args.extend(started);
            super::super::compose(&name, &args)
        })
        .await??;
        pitch_a.store(880, Ordering::Relaxed);
        pitch_b.store(1100, Ordering::Relaxed);
        let baseline = timeout(
            Duration::from_secs(12),
            both_ways(&mut left, &mut right, 880, 1100),
        )
        .await;
        println!(
            "#356 {services:?} restart before app fallback: new-tone duplex {}, room states {:?}/{:?}",
            if matches!(baseline, Ok(Ok(()))) {
                "flowing"
            } else {
                "not restored in 12 seconds"
            },
            left.connection_state(),
            right.connection_state()
        );
        let _ = timeout(Duration::from_secs(15), left.disconnect()).await;
        let _ = timeout(Duration::from_secs(15), right.disconnect()).await;
        left = fresh_room(a, &scope.id).await?;
        right = fresh_room(b, &scope.id).await?;
        tokio::try_join!(left.publish_audio(mic_a), right.publish_audio(mic_b))?;
        let new_screen = left
            .publish_screen(screen::NAME, screen::options(), video)
            .await?
            .sid();
        let new_sound = left
            .publish_stream_audio("Synthetic shared audio", sound)
            .await?
            .sid();
        ensure!(old_screen != new_screen, "rejoin reused a screen track ID");
        timeout(hang_guard(), both_ways(&mut left, &mut right, 880, 1100))
            .await
            .context("encrypted duplex after service restart")??;
        timeout(
            hang_guard(),
            screen::hear(&mut right, TrackSource::Screenshare),
        )
        .await
        .context("decrypted screen frames after service restart")??;
        ensure!(
            left.identity() == owner,
            "rejoin changed stable device identity"
        );
        let new_session = right
            .participants()
            .into_iter()
            .find(|(peer, _)| peer.identity() == owner)
            .context("screen owner absent after restart")?
            .0
            .sid();
        ensure!(
            old_session != new_session,
            "rejoin reused participant session"
        );
        let recovered_screen = screen::find(&mut right, TrackSource::Screenshare).await?;
        ensure!(
            recovered_screen.participant == owner && recovered_screen.sid == new_screen,
            "recovered screen belongs to a different owner or publication"
        );
        right
            .set_video_watches(std::slice::from_ref(&new_screen))
            .await?;
        ensure!(
            !right.is_listening(&new_sound),
            "restart restored stale Listen consent"
        );
        ensure!(
            !right
                .remote_audio_tracks()
                .iter()
                .any(|(_, sid, _)| sid == &new_sound),
            "screen recovery subscribed to shared sound without renewed Listen"
        );
        super::consent::listen(&mut right, &new_sound).await?;
        timeout(
            hang_guard(),
            rusttalk_bot::voice::hear_track(&mut right, &new_sound, 1320),
        )
        .await??;
        roster(a, &scope.id, 2).await?;
        tokio::try_join!(left.leave(), right.leave())?;
        roster(a, &scope.id, 0).await?;
        senders.abort_all();
        println!(
            "#356 {services:?} restart: fresh auth/key, encrypted duplex and screen frames, changed track SID, cancelled third client absent, final roster empty"
        );
    }
    // Access revoked during an outage must end recovery, not retry a cached token.
    let person = rusttalk_core::auth::encode(c.identity.certificate.person_id());
    a.request(reqwest::Method::PUT, &format!("/v1/bans/{person}"))
        .json(&serde_json::json!({"ban_ip": false}))
        .send()
        .await?
        .error_for_status()?;
    let denied = c
        .reconnect()
        .await
        .expect_err("banned client reauthenticated");
    ensure!(
        denied
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::status)
            == Some(reqwest::StatusCode::FORBIDDEN),
        "ban was not a terminal authorization denial"
    );
    println!("#356 banned client's fresh authentication refused with 403");
    Ok(())
}
