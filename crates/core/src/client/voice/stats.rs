use std::{collections::HashMap, time::Instant};

use anyhow::Result;
use livekit::{Room, e2ee::EncryptionType, webrtc::stats::RtcStats};

use super::{ParticipantIdentity, TrackSid};

/// A WebRTC snapshot. RTT and jitter are milliseconds.
pub struct VoiceStats {
    pub ping_ms: Option<f64>,
    pub tracks: Vec<TrackStats>,
}

/// One published or subscribed media track. Loss counters are cumulative.
pub struct TrackStats {
    pub participant: ParticipantIdentity,
    pub sid: TrackSid,
    pub outbound: bool,
    pub jitter_ms: Option<f64>,
    pub packets_lost: Option<i64>,
    pub bitrate_bps: Option<f64>,
}

/// Byte counters by WebRTC stat id, one per simulcast layer.
type Samples = HashMap<String, (u64, Instant)>;

#[derive(Default)]
pub(super) struct Sampler {
    previous: Samples,
}

impl Sampler {
    pub async fn snapshot(&mut self, room: &Room) -> Result<VoiceStats> {
        let session = room.get_stats().await?;
        let ping_ms = ping(&session.subscriber_stats).or_else(|| ping(&session.publisher_stats));
        let mut snapshot = VoiceStats {
            ping_ms,
            tracks: Vec::new(),
        };
        let local = room.local_participant();
        let mut samples = HashMap::new();
        for (sid, publication) in local.track_publications() {
            if let Some(track) = publication.track() {
                let report = track.get_stats().await?;
                let (jitter_ms, packets_lost, bitrate_bps) =
                    measure(&report, true, &self.previous, &mut samples);
                snapshot.tracks.push(TrackStats {
                    participant: local.identity(),
                    sid,
                    outbound: true,
                    jitter_ms,
                    packets_lost,
                    bitrate_bps,
                });
            }
        }
        for participant in room.remote_participants().into_values() {
            for (sid, publication) in participant.track_publications() {
                if publication.encryption_type() == EncryptionType::Gcm
                    && let Some(track) = publication.track()
                {
                    let report = track.get_stats().await?;
                    let (jitter_ms, packets_lost, bitrate_bps) =
                        measure(&report, false, &self.previous, &mut samples);
                    snapshot.tracks.push(TrackStats {
                        participant: participant.identity(),
                        sid,
                        outbound: false,
                        jitter_ms,
                        packets_lost,
                        bitrate_bps,
                    });
                }
            }
        }
        self.previous = samples;
        Ok(snapshot)
    }
}

/// Jitter (worst layer), packets lost and bitrate (summed over simulcast layers) of one track.
fn measure(
    report: &[RtcStats],
    outbound: bool,
    previous: &Samples,
    samples: &mut Samples,
) -> (Option<f64>, Option<i64>, Option<f64>) {
    let max = |a: Option<f64>, b: f64| Some(a.map_or(b, |a| a.max(b)));
    let (mut jitter, mut lost, mut bitrate) = (None, None, None);
    let now = Instant::now();
    for stat in report {
        let (id, bytes) = match stat {
            RtcStats::InboundRtp(stat) if !outbound => {
                jitter = max(jitter, stat.received.jitter * 1000.0);
                lost = Some(lost.unwrap_or(0) + stat.received.packets_lost);
                (&stat.rtc.id, stat.inbound.bytes_received)
            }
            RtcStats::OutboundRtp(stat) if outbound => (&stat.rtc.id, stat.sent.bytes_sent),
            RtcStats::RemoteInboundRtp(stat) if outbound => {
                jitter = max(jitter, stat.received.jitter * 1000.0);
                lost = Some(lost.unwrap_or(0) + stat.received.packets_lost);
                continue;
            }
            _ => continue,
        };
        if let Some((previous_bytes, at)) = previous.get(id)
            && let Some(delta) = bytes.checked_sub(*previous_bytes)
        {
            let elapsed = now.duration_since(*at).as_secs_f64();
            if elapsed > 0.0 {
                bitrate = Some(bitrate.unwrap_or(0.0) + 8.0 * delta as f64 / elapsed);
            }
        }
        samples.insert(id.clone(), (bytes, now));
    }
    (jitter, lost, bitrate)
}

fn ping(report: &[RtcStats]) -> Option<f64> {
    let selected = report.iter().find_map(|stat| match stat {
        RtcStats::Transport(stat) if !stat.transport.selected_candidate_pair_id.is_empty() => {
            Some(&stat.transport.selected_candidate_pair_id)
        }
        _ => None,
    })?;
    report.iter().find_map(|stat| match stat {
        RtcStats::CandidatePair(stat)
            if &stat.rtc.id == selected && stat.candidate_pair.responses_received > 0 =>
        {
            Some(stat.candidate_pair.current_round_trip_time * 1000.0)
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use livekit::webrtc::stats::{OutboundRtpStats, RemoteInboundRtpStats};

    use super::*;

    fn layer(id: &str, bytes: u64) -> RtcStats {
        let mut stat = OutboundRtpStats::default();
        stat.rtc.id = id.into();
        stat.sent.bytes_sent = bytes;
        RtcStats::OutboundRtp(stat)
    }

    fn loss(lost: i64) -> RtcStats {
        let mut stat = RemoteInboundRtpStats::default();
        stat.received.packets_lost = lost;
        RtcStats::RemoteInboundRtp(stat)
    }

    #[test]
    fn simulcast_layers_add_up() {
        let at = Instant::now() - Duration::from_secs(1);
        let previous = HashMap::from([("q".into(), (0, at)), ("h".into(), (0, at))]);
        let report = [layer("q", 1_000), layer("h", 9_000), loss(2), loss(3)];
        let mut samples = HashMap::new();
        let (_, lost, bitrate) = measure(&report, true, &previous, &mut samples);
        assert_eq!(lost, Some(5));
        let bitrate = bitrate.unwrap();
        assert!((75_000.0..=80_000.0).contains(&bitrate), "{bitrate}");
        assert_eq!(samples.len(), 2);
        assert_eq!(samples["h"].0, 9_000);
    }

    #[test]
    fn first_sample_has_no_bitrate() {
        let mut samples = HashMap::new();
        let (_, lost, bitrate) = measure(&[layer("q", 1_000)], true, &HashMap::new(), &mut samples);
        assert_eq!((lost, bitrate), (None, None));
        assert_eq!(samples.len(), 1);
    }
}
