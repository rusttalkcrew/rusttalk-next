//! What LiveKit is carrying right now, for server statistics (#214): counts only.

use std::time::{Duration, Instant};

use livekit_protocol::{TrackInfo, TrackSource};
use rusttalk_core::auth::{StatsDirection, StatsVoice};
use serde::Deserialize;

use super::{DATA_PREFIX, LiveKit};
use crate::ApiResult;

/// Answers are shared this long, so several open statistics pages cost LiveKit one look.
const FRESH: Duration = Duration::from_secs(2);
/// A rate needs two looks at most this far apart; older ones say too little about now.
const RATE_WINDOW: Duration = Duration::from_secs(10);
/// LiveKit's metrics page is a few hundred KiB at most; anything bigger is refused.
const METRICS_LIMIT: usize = 4 << 20;

#[derive(Default)]
pub struct Usage {
    /// The last answer and when it was asked for.
    last: Option<(Instant, Option<StatsVoice>, Option<StatsDirection>)>,
    /// The previous media byte totals, for the rate.
    totals: Option<(Instant, u64, u64)>,
}

#[derive(Deserialize)]
struct Roster {
    #[serde(default)]
    participants: Vec<Participant>,
}

#[derive(Deserialize)]
struct Participant {
    #[serde(default)]
    state: String,
    #[serde(default)]
    tracks: Vec<TrackInfo>,
}

impl LiveKit {
    /// People in voice and their streams, and media traffic, as LiveKit last told them.
    /// Either is `None` while LiveKit doesn't answer.
    pub async fn usage(&self) -> (Option<StatsVoice>, Option<StatsDirection>) {
        let mut usage = self.usage.lock().await;
        if let Some((at, voice, media)) = &usage.last
            && at.elapsed() < FRESH
        {
            return (*voice, *media);
        }
        let asked = Instant::now();
        let voice = self.voice_usage().await.ok();
        let media = self.media_totals().await.map(|(received, sent)| {
            let rate = usage
                .totals
                .filter(|(at, ..)| asked.duration_since(*at) <= RATE_WINDOW)
                .map_or((0, 0), |(at, before_in, before_out)| {
                    let millis = asked.duration_since(at).as_millis().max(1) as u64;
                    let per_sec =
                        |now: u64, before: u64| now.saturating_sub(before) * 1000 / millis;
                    (per_sec(received, before_in), per_sec(sent, before_out))
                });
            usage.totals = Some((asked, received, sent));
            StatsDirection {
                received,
                sent,
                received_per_sec: rate.0,
                sent_per_sec: rate.1,
            }
        });
        usage.last = Some((asked, voice, media));
        (voice, media)
    }

    async fn voice_usage(&self) -> ApiResult<StatsVoice> {
        let mut voice = StatsVoice::default();
        for room in self.rooms().await? {
            // Data rooms carry live file transfers, not calls.
            if room.starts_with(DATA_PREFIX) {
                continue;
            }
            let response = self
                .room_service(
                    "ListParticipants",
                    &room,
                    serde_json::json!({ "room": room }),
                )
                .await?;
            if !response.status().is_success() {
                continue;
            }
            let Ok(roster) = response.json::<Roster>().await else {
                continue;
            };
            let active: Vec<_> = (roster.participants.iter())
                .filter(|participant| participant.state == "ACTIVE")
                .collect();
            if active.is_empty() {
                continue;
            }
            voice.channels += 1;
            voice.people += active.len() as u64;
            for track in active.iter().flat_map(|participant| &participant.tracks) {
                count(&mut voice, track);
            }
        }
        Ok(voice)
    }

    /// Media bytes in and out since LiveKit started, from its metrics on the stack network.
    async fn media_totals(&self) -> Option<(u64, u64)> {
        let mut response = self
            .http
            .get("http://livekit:6789/metrics")
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let mut text = Vec::new();
        while let Some(chunk) = response.chunk().await.ok()? {
            if text.len() + chunk.len() > METRICS_LIMIT {
                return None;
            }
            text.extend_from_slice(&chunk);
        }
        media_bytes(std::str::from_utf8(&text).ok()?)
    }
}

/// Unmuted streams by kind.
fn count(voice: &mut StatsVoice, track: &TrackInfo) {
    if track.muted {
        return;
    }
    match track.source() {
        TrackSource::Microphone => voice.audio += 1,
        TrackSource::Camera => voice.camera += 1,
        TrackSource::ScreenShare => voice.screen += 1,
        _ => {}
    }
}

/// The sums of `livekit_packet_bytes` by direction, over every other label.
fn media_bytes(metrics: &str) -> Option<(u64, u64)> {
    let (mut received, mut sent, mut seen) = (0.0, 0.0, false);
    for line in metrics.lines() {
        let Some(rest) = line.strip_prefix("livekit_packet_bytes{") else {
            continue;
        };
        let Some((labels, value)) = rest.split_once('}') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<f64>() else {
            continue;
        };
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        seen = true;
        if labels.contains("direction=\"incoming\"") {
            received += value;
        } else if labels.contains("direction=\"outgoing\"") {
            sent += value;
        }
    }
    seen.then_some((received as u64, sent as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_bytes_add_up_by_direction() {
        let metrics = "\
# HELP livekit_packet_bytes packet bytes
# TYPE livekit_packet_bytes counter
livekit_packet_bytes{country=\"\",direction=\"incoming\",node_id=\"n\",node_type=\"SERVER\",transmission=\"initial\"} 1000
livekit_packet_bytes{country=\"\",direction=\"incoming\",node_id=\"n\",node_type=\"SERVER\",transmission=\"retransmit\"} 24
livekit_packet_bytes{country=\"\",direction=\"outgoing\",node_id=\"n\",node_type=\"SERVER\",transmission=\"initial\"} 3.5e+06
livekit_packet_total{country=\"\",direction=\"incoming\",node_id=\"n\",node_type=\"SERVER\",transmission=\"initial\"} 99
";
        assert_eq!(media_bytes(metrics), Some((1024, 3_500_000)));
        // Nothing sent yet, or no such metric: unknown rather than zero.
        assert_eq!(media_bytes("livekit_packet_total{} 5\n"), None);
        // Broken or hostile values are skipped.
        let odd = "livekit_packet_bytes{direction=\"incoming\"} NaN\n\
                   livekit_packet_bytes{direction=\"incoming\"} -5\n\
                   livekit_packet_bytes{direction=\"incoming\" 7\n\
                   livekit_packet_bytes{direction=\"outgoing\"} +Inf\n\
                   livekit_packet_bytes{direction=\"outgoing\"} 9\n";
        assert_eq!(media_bytes(odd), Some((0, 9)));
    }

    #[test]
    fn streams_count_by_kind_and_skip_muted_ones() {
        let roster: Roster = serde_json::from_value(serde_json::json!({
            "participants": [{
                "identity": "a",
                "state": "ACTIVE",
                "tracks": [
                    {"sid": "1", "type": "AUDIO", "source": "MICROPHONE"},
                    {"sid": "2", "type": "VIDEO", "source": "CAMERA"},
                    {"sid": "3", "type": "VIDEO", "source": "SCREEN_SHARE"},
                    {"sid": "4", "type": "AUDIO", "source": "SCREEN_SHARE_AUDIO"},
                ],
            }, {
                "identity": "b",
                "state": "ACTIVE",
                "tracks": [{"sid": "5", "type": "AUDIO", "source": "MICROPHONE", "muted": true}],
            }],
        }))
        .unwrap();
        let mut voice = StatsVoice::default();
        for track in roster.participants.iter().flat_map(|p| &p.tracks) {
            count(&mut voice, track);
        }
        assert_eq!((voice.audio, voice.camera, voice.screen), (1, 1, 1));
    }
}
