use anyhow::{Context, Result, ensure};
use rusttalk_core::{
    capture::Capture,
    client::voice::{ParticipantIdentity, TrackSid, TrackSource, VoiceRoom},
};

use crate::voice::measure_tone;

#[cfg(target_os = "linux")]
mod linux;

const TONES: [(&str, u32); 2] = [("RustTalk Tone A", 440), ("RustTalk Tone B", 660)];

pub struct AppTones {
    _captures: Vec<Capture>,
    tracks: Vec<TrackSid>,
    #[cfg(target_os = "linux")]
    _playback: linux::Playback,
}

impl AppTones {
    pub async fn stop(mut self, room: &VoiceRoom) -> Result<()> {
        let tracks = std::mem::take(&mut self.tracks);
        drop(self);
        for sid in tracks {
            room.unpublish_audio(&sid).await?;
        }
        Ok(())
    }
}

pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    return linux::available();
    #[cfg(not(target_os = "linux"))]
    false
}

/// Publish two separate local PipeWire playback streams.
pub async fn share(room: &VoiceRoom) -> Result<AppTones> {
    #[cfg(target_os = "linux")]
    {
        let (playback, apps) = tokio::task::spawn_blocking(linux::play).await??;
        let mut captures = Vec::new();
        let mut tracks = Vec::new();
        for app in apps {
            let source = rusttalk_core::capture::audio_source();
            tracks.push(
                room.publish_app_audio(&app.name, source.clone())
                    .await?
                    .sid(),
            );
            captures.push(
                tokio::task::spawn_blocking(move || {
                    rusttalk_core::capture::capture_to_source(&app, source)
                })
                .await??,
            );
        }
        Ok(AppTones {
            _captures: captures,
            tracks,
            _playback: playback,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = room;
        anyhow::bail!("the two-app bot sender requires PipeWire on Linux");
    }
}

/// Check two distinct encrypted app tracks and independently scale each one's PCM.
pub async fn hear(room: &mut VoiceRoom, peer: Option<&ParticipantIdentity>) -> Result<()> {
    let mut tracks = loop {
        let tracks: Vec<_> = room
            .remote_audio_tracks()
            .into_iter()
            .filter(|(id, _, track)| {
                peer.is_none_or(|peer| peer == id)
                    && track.source() == TrackSource::ScreenshareAudio
            })
            .collect();
        if tracks.len() >= 2 {
            break tracks;
        }
        room.next_event()
            .await
            .context("room closed waiting for app tracks")?;
    };
    ensure!(tracks.len() == 2, "expected two separate app tracks");
    let measure = |name, hz, gain| {
        let i = tracks
            .iter()
            .position(|(_, _, track)| room.app_audio_name(track).as_deref() == Some(name))
            .context("missing decrypted app track name")?;
        let (_, _, track) = tracks.swap_remove(i);
        let wire = track.name();
        ensure!(wire != name, "LiveKit received a plaintext app name");
        Ok(measure_tone(track, hz, gain))
    };
    let mut measure = measure;
    let first = measure(TONES[0].0, TONES[0].1, 1.0)?;
    let second = measure(TONES[1].0, TONES[1].1, 0.25)?;
    let ((raw_a, scaled_a), (raw_b, scaled_b)) = tokio::try_join!(first, second)?;
    ensure!(
        (0.99..1.01).contains(&(scaled_a / raw_a))
            && (0.24..0.26).contains(&(scaled_b / raw_b))
            && scaled_b < scaled_a * 0.5,
        "expected the second app stream at quarter volume, got {:.2} and {:.2} PCM",
        scaled_a,
        scaled_b
    );
    println!(
        "W09 two encrypted ScreenshareAudio tracks, 440 Hz and 660 Hz, independent gain (mean levels {scaled_a:.0} and {scaled_b:.0})"
    );
    Ok(())
}
