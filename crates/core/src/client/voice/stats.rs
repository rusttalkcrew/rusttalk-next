use std::{collections::HashMap, time::Instant};

use anyhow::Result;
use livekit::{Room, e2ee::EncryptionType, webrtc::stats::RtcStats};

use super::{ParticipantIdentity, TrackSid, TrackSource};

/// A WebRTC snapshot. RTT and jitter are milliseconds.
pub struct VoiceStats {
    pub ping_ms: Option<f64>,
    pub tracks: Vec<TrackStats>,
    pub traffic: CallTraffic,
}

/// One published or subscribed media track. Loss counters are cumulative.
pub struct TrackStats {
    pub participant: ParticipantIdentity,
    pub sid: TrackSid,
    pub outbound: bool,
    pub jitter_ms: Option<f64>,
    pub packets_lost: Option<i64>,
    /// Received packets, inbound tracks only; cumulative like the loss.
    pub packets_received: Option<u64>,
    pub bitrate_bps: Option<f64>,
    /// Video only: the picture received, or the biggest layer sent.
    pub video: Option<VideoFormat>,
}

/// The size and frame rate a video track is running at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VideoFormat {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

#[derive(Clone, Copy, Default)]
pub struct Traffic {
    pub bytes: u64,
    pub bps: f64,
}

/// Locally measured RTP traffic, with bytes accumulated for the life of the room.
#[derive(Clone, Copy, Default)]
pub struct CallTraffic {
    sources: [[Traffic; 2]; 5],
}

impl CallTraffic {
    /// Upload and download, in that order.
    pub fn total(&self) -> [Traffic; 2] {
        let mut total = [Traffic::default(); 2];
        for pair in self.sources {
            for (total, source) in total.iter_mut().zip(pair) {
                total.bytes += source.bytes;
                total.bps += source.bps;
            }
        }
        total
    }

    /// Upload and download for one publication source.
    pub fn for_source(&self, source: TrackSource) -> [Traffic; 2] {
        self.sources[Self::index(source)]
    }

    fn index(source: TrackSource) -> usize {
        match source {
            TrackSource::Microphone => 0,
            TrackSource::Camera => 1,
            TrackSource::Screenshare => 2,
            TrackSource::ScreenshareAudio => 3,
            TrackSource::Unknown => 4,
        }
    }

    fn add(&mut self, source: TrackSource, outbound: bool, track: &TrackStats, bytes: u64) {
        let traffic = &mut self.sources[Self::index(source)][usize::from(!outbound)];
        traffic.bytes += bytes;
        traffic.bps += track.bitrate_bps.unwrap_or_default();
    }

    fn clear_rates(&mut self) {
        for traffic in self.sources.iter_mut().flatten() {
            traffic.bps = 0.0;
        }
    }
}

/// Byte counters by WebRTC stat id, one per simulcast layer.
type Samples = HashMap<String, (u64, Instant)>;

#[derive(Default)]
pub(super) struct Sampler {
    previous: HashMap<TrackSid, Samples>,
    traffic: CallTraffic,
}

impl Sampler {
    pub async fn snapshot(&mut self, room: &Room) -> Result<VoiceStats> {
        let session = room.get_stats().await?;
        let ping_ms = ping(&session.subscriber_stats).or_else(|| ping(&session.publisher_stats));
        let mut snapshot = VoiceStats {
            ping_ms,
            tracks: Vec::new(),
            traffic: self.traffic,
        };
        snapshot.traffic.clear_rates();
        let local = room.local_participant();
        let mut samples = HashMap::new();
        for (sid, publication) in local.track_publications() {
            let mut counters = self.previous.get(&sid).cloned().unwrap_or_default();
            if let Some(track) = publication.track() {
                let report = track.get_stats().await?;
                snapshot.record(
                    local.identity(),
                    &sid,
                    publication.source(),
                    true,
                    &report,
                    &mut counters,
                );
            }
            samples.insert(sid, counters);
        }
        for participant in room.remote_participants().into_values() {
            for (sid, publication) in participant.track_publications() {
                if publication.encryption_type() != EncryptionType::Gcm {
                    continue;
                }
                // A publication can remain while its subscription is paused or switched.
                let mut counters = self.previous.get(&sid).cloned().unwrap_or_default();
                if let Some(track) = publication.track() {
                    let report = track.get_stats().await?;
                    snapshot.record(
                        participant.identity(),
                        &sid,
                        publication.source(),
                        false,
                        &report,
                        &mut counters,
                    );
                }
                samples.insert(sid, counters);
            }
        }
        self.previous = samples;
        self.traffic = snapshot.traffic;
        Ok(snapshot)
    }
}

impl VoiceStats {
    fn record(
        &mut self,
        participant: ParticipantIdentity,
        sid: &TrackSid,
        source: TrackSource,
        outbound: bool,
        report: &[RtcStats],
        counters: &mut Samples,
    ) {
        let mut track = TrackStats {
            participant,
            sid: sid.clone(),
            outbound,
            jitter_ms: None,
            packets_lost: None,
            packets_received: None,
            bitrate_bps: None,
            video: None,
        };
        let bytes = measure(report, &mut track, counters);
        self.traffic.add(source, outbound, &track, bytes);
        self.tracks.push(track);
    }
}

/// Update one track and return the newly observed bytes across its layers.
fn measure(report: &[RtcStats], track: &mut TrackStats, samples: &mut Samples) -> u64 {
    let max = |a: Option<f64>, b: f64| Some(a.map_or(b, |a| a.max(b)));
    let mut new_bytes = 0;
    let now = Instant::now();
    // Audio reports no frame size. Of several sent layers, show the biggest.
    let video = |track: &mut TrackStats, width, height, fps| {
        if width > track.video.map_or(0, |video| video.width) {
            track.video = Some(VideoFormat { width, height, fps });
        }
    };
    for stat in report {
        let (id, bytes) = match stat {
            RtcStats::InboundRtp(stat) if !track.outbound => {
                let i = &stat.inbound;
                video(track, i.frame_width, i.frame_height, i.frames_per_second);
                track.jitter_ms = max(track.jitter_ms, stat.received.jitter * 1000.0);
                *track.packets_lost.get_or_insert(0) += stat.received.packets_lost;
                *track.packets_received.get_or_insert(0) += stat.received.packets_received;
                (&stat.rtc.id, stat.inbound.bytes_received)
            }
            RtcStats::OutboundRtp(stat) if track.outbound => {
                let o = &stat.outbound;
                video(track, o.frame_width, o.frame_height, o.frames_per_second);
                (&stat.rtc.id, stat.sent.bytes_sent)
            }
            RtcStats::RemoteInboundRtp(stat) if track.outbound => {
                track.jitter_ms = max(track.jitter_ms, stat.received.jitter * 1000.0);
                *track.packets_lost.get_or_insert(0) += stat.received.packets_lost;
                continue;
            }
            _ => continue,
        };
        if let Some((previous_bytes, at)) = samples.insert(id.clone(), (bytes, now))
            && let Some(delta) = bytes.checked_sub(previous_bytes)
        {
            new_bytes += delta;
            let elapsed = now.duration_since(at).as_secs_f64();
            if elapsed > 0.0 {
                *track.bitrate_bps.get_or_insert(0.0) += 8.0 * delta as f64 / elapsed;
            }
        } else {
            // A new stream or reset starts a fresh byte count with no rate baseline.
            new_bytes += bytes;
        }
    }
    new_bytes
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

    use livekit::webrtc::stats::{InboundRtpStats, OutboundRtpStats, RemoteInboundRtpStats};

    use super::*;

    fn layer(id: &str, bytes: u64) -> RtcStats {
        let mut stat = OutboundRtpStats::default();
        stat.rtc.id = id.into();
        stat.sent.bytes_sent = bytes;
        if id == "h" {
            (stat.outbound.frame_width, stat.outbound.frame_height) = (1920, 1080);
            stat.outbound.frames_per_second = 60.0;
        } else if id == "q" {
            (stat.outbound.frame_width, stat.outbound.frame_height) = (960, 540);
            stat.outbound.frames_per_second = 30.0;
        }
        RtcStats::OutboundRtp(stat)
    }

    fn loss(lost: i64) -> RtcStats {
        let mut stat = RemoteInboundRtpStats::default();
        stat.received.packets_lost = lost;
        RtcStats::RemoteInboundRtp(stat)
    }

    fn record(
        snapshot: &mut VoiceStats,
        source: TrackSource,
        outbound: bool,
        report: &[RtcStats],
        samples: &mut Samples,
    ) {
        snapshot.record(
            ParticipantIdentity("device".into()),
            &TrackSid::try_from("TR_test".to_owned()).unwrap(),
            source,
            outbound,
            report,
            samples,
        );
    }

    fn snapshot(traffic: CallTraffic) -> VoiceStats {
        let mut snapshot = VoiceStats {
            ping_ms: None,
            tracks: Vec::new(),
            traffic,
        };
        snapshot.traffic.clear_rates();
        snapshot
    }

    #[test]
    fn simulcast_layers_add_up() {
        let at = Instant::now() - Duration::from_secs(1);
        let mut samples = HashMap::from([("q".into(), (0, at)), ("h".into(), (0, at))]);
        let report = [layer("q", 1_000), layer("h", 9_000), loss(2), loss(3)];
        let mut stats = snapshot(CallTraffic::default());
        record(&mut stats, TrackSource::Camera, true, &report, &mut samples);
        assert_eq!(stats.tracks[0].packets_lost, Some(5));
        assert_eq!(
            stats.tracks[0].video,
            Some(VideoFormat {
                width: 1920,
                height: 1080,
                fps: 60.0
            })
        );
        let bitrate = stats.tracks[0].bitrate_bps.unwrap();
        assert!((75_000.0..=80_000.0).contains(&bitrate), "{bitrate}");
        assert_eq!(stats.traffic.total()[0].bps, bitrate);
        assert_eq!(stats.traffic.total()[0].bytes, 10_000);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples["h"].0, 9_000);
    }

    #[test]
    fn traffic_accumulates_across_restarts_and_inactive_samples() {
        let mut camera = HashMap::new();
        let mut app_audio = HashMap::new();
        let mut traffic = CallTraffic::default();
        for (camera_bytes, expected) in [
            (Some(100), 100),
            (Some(120), 120),
            (None, 120),
            (Some(120), 120),
            (Some(30), 150),
            (Some(50), 170),
            (None, 170),
        ] {
            let mut stats = snapshot(traffic);
            if let Some(bytes) = camera_bytes {
                record(
                    &mut stats,
                    TrackSource::Camera,
                    true,
                    &[layer("rtp", bytes)],
                    &mut camera,
                );
                if [100, 30].contains(&bytes) {
                    assert_eq!(stats.tracks[0].bitrate_bps, None);
                }
            }
            let mut inbound = InboundRtpStats::default();
            inbound.rtc.id = "rtp".into();
            inbound.inbound.bytes_received = 200;
            record(
                &mut stats,
                TrackSource::ScreenshareAudio,
                false,
                &[RtcStats::InboundRtp(inbound)],
                &mut app_audio,
            );
            // Audio has no picture.
            assert_eq!(stats.tracks.last().unwrap().video, None);
            traffic = stats.traffic;
            assert_eq!(traffic.total().map(|value| value.bytes), [expected, 200]);
            assert_eq!(
                traffic
                    .for_source(TrackSource::Camera)
                    .map(|value| value.bytes),
                [expected, 0]
            );
            assert_eq!(
                traffic
                    .for_source(TrackSource::ScreenshareAudio)
                    .map(|value| value.bytes),
                [0, 200]
            );
            if camera_bytes.is_none() {
                assert_eq!(traffic.total()[0].bps, 0.0);
            }
        }
        assert_eq!(
            Sampler::default().traffic.total().map(|value| value.bytes),
            [0, 0]
        );
    }
}
