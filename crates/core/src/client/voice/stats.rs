use std::{collections::HashMap, time::Instant};

use anyhow::Result;
use livekit::{Room, webrtc::stats::RtcStats};

use super::{ParticipantIdentity, RemoteAudioTrack, TrackSid};

/// A WebRTC snapshot. RTT and jitter are milliseconds.
pub struct VoiceStats {
    pub ping_ms: Option<f64>,
    pub tracks: Vec<TrackStats>,
}

/// One published or subscribed audio track. Loss counters are cumulative.
pub struct TrackStats {
    pub participant: ParticipantIdentity,
    pub sid: TrackSid,
    pub outbound: bool,
    pub jitter_ms: Option<f64>,
    pub packets_lost: Option<i64>,
    pub bitrate_bps: Option<f64>,
}

#[derive(Default)]
pub(super) struct Sampler {
    previous: HashMap<TrackSid, (String, u64, Instant)>,
}

impl Sampler {
    pub async fn snapshot(
        &mut self,
        room: &Room,
        remote: Vec<(ParticipantIdentity, TrackSid, RemoteAudioTrack)>,
    ) -> Result<VoiceStats> {
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
                snapshot.tracks.push(self.track(
                    local.identity(),
                    sid,
                    true,
                    &report,
                    &mut samples,
                ));
            }
        }
        for (identity, sid, track) in remote {
            let report = track.get_stats().await?;
            snapshot
                .tracks
                .push(self.track(identity, sid, false, &report, &mut samples));
        }
        self.previous = samples;
        Ok(snapshot)
    }

    fn track(
        &self,
        participant: ParticipantIdentity,
        sid: TrackSid,
        outbound: bool,
        report: &[RtcStats],
        samples: &mut HashMap<TrackSid, (String, u64, Instant)>,
    ) -> TrackStats {
        let mut result = TrackStats {
            participant,
            outbound,
            sid,
            jitter_ms: None,
            packets_lost: None,
            bitrate_bps: None,
        };
        for stat in report {
            let (id, bytes) = match stat {
                RtcStats::InboundRtp(stat) if !outbound => {
                    result.jitter_ms = Some(stat.received.jitter * 1000.0);
                    result.packets_lost = Some(stat.received.packets_lost);
                    (&stat.rtc.id, stat.inbound.bytes_received)
                }
                RtcStats::OutboundRtp(stat) if outbound => (&stat.rtc.id, stat.sent.bytes_sent),
                RtcStats::RemoteInboundRtp(stat) if outbound => {
                    result.jitter_ms = Some(stat.received.jitter * 1000.0);
                    result.packets_lost = Some(stat.received.packets_lost);
                    continue;
                }
                _ => continue,
            };
            let now = Instant::now();
            if let Some((previous_id, previous_bytes, at)) = self.previous.get(&result.sid)
                && id == previous_id
                && let Some(delta) = bytes.checked_sub(*previous_bytes)
            {
                let elapsed = now.duration_since(*at).as_secs_f64();
                if elapsed > 0.0 {
                    result.bitrate_bps = Some(8.0 * delta as f64 / elapsed);
                }
            }
            samples.insert(result.sid.clone(), (id.clone(), bytes, now));
        }
        result
    }
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
