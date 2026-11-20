use std::{f32::consts::TAU, time::Duration};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use livekit::webrtc::{audio_frame::AudioFrame, audio_source::AudioSourceOptions};
use rusttalk_core::client::voice::{
    NativeAudioSource, NativeAudioStream, ParticipantIdentity, RemoteAudioTrack, TrackSource,
    VoiceRoom,
};
use tokio::time::{MissedTickBehavior, interval};

const RATE: u32 = 48_000;
const FRAME: u32 = RATE / 100;

pub fn source() -> NativeAudioSource {
    NativeAudioSource::new(AudioSourceOptions::default(), RATE, 1, 0)
}

pub async fn send_tone(source: NativeAudioSource, frequency: impl Fn() -> u32) -> Result<()> {
    let mut tick = interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut frame = AudioFrame::new(RATE, 1, FRAME);
    let mut sample = 0;
    loop {
        tick.tick().await;
        let hz = frequency();
        for output in frame.data.to_mut() {
            *output = (12_000.0 * (TAU * hz as f32 * sample as f32 / RATE as f32).sin()) as i16;
            sample = (sample + 1) % RATE;
        }
        source
            .capture_frame(&frame)
            .await
            .context("capture synthetic audio")?;
    }
}

/// Decode PCM from the peer and wait for a sustained tone near the expected frequency.
pub async fn hear_tone(
    room: &mut VoiceRoom,
    peer: Option<&ParticipantIdentity>,
    hz: u32,
) -> Result<()> {
    let track = loop {
        if let Some((_, _, track)) =
            room.remote_audio_tracks()
                .into_iter()
                .find(|(id, _, track)| {
                    peer.is_none_or(|peer| peer == id) && track.source() == TrackSource::Microphone
                })
        {
            break track;
        }
        room.next_event()
            .await
            .context("room closed waiting for remote audio")?;
    };
    measure_tone(track, hz, 500.0).await?;
    Ok(())
}

/// Wait for a 100 ms window near `hz` with a mean absolute level above `min_level`.
pub(crate) async fn measure_tone(track: RemoteAudioTrack, hz: u32, min_level: f32) -> Result<f32> {
    let mut stream = NativeAudioStream::new(track.rtc_track(), RATE as i32, 1);
    let mut window = Vec::with_capacity(RATE as usize / 10);
    while let Some(frame) = stream.next().await {
        window.extend_from_slice(&frame.data);
        if window.len() < RATE as usize / 10 {
            continue;
        }
        let crossings = window.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count();
        let frequency = crossings as f32 * RATE as f32 / window.len() as f32;
        let mean_abs =
            window.iter().map(|x| f32::from(*x).abs()).sum::<f32>() / window.len() as f32;
        if mean_abs > min_level && (frequency - hz as f32).abs() <= 25.0 {
            return Ok(mean_abs);
        }
        window.clear();
    }
    anyhow::bail!("remote audio ended before hearing {hz} Hz")
}
