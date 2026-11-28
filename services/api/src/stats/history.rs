//! The last 24 hours of server statistics, in memory only (#214).

use std::{collections::VecDeque, sync::Mutex, time::Duration};

use rusttalk_core::auth::{StatsHistory, StatsPoint};

use crate::{App, store};

/// How often a point is taken.
pub const STEP: Duration = Duration::from_secs(10);
/// A day of points.
const KEEP: usize = 8640;
/// The ranges the page offers, in seconds.
pub const RANGES: [i64; 4] = [300, 3600, 21_600, 86_400];
/// Points a range is cut into, at most.
const BUCKETS: i64 = 60;

#[derive(Clone, Copy, Default)]
struct Point {
    at: i64,
    /// The API's bytes received and sent since start.
    control: (u64, u64),
    /// LiveKit's, when it answered.
    media: Option<(u64, u64)>,
    connected: u64,
    voice: Option<u64>,
}

#[derive(Default)]
pub struct History(Mutex<VecDeque<Point>>);

impl History {
    fn push(&self, point: Point) {
        let mut points = self.0.lock().expect("stats history");
        if points.len() == KEEP {
            points.pop_front();
        }
        points.push_back(point);
    }

    /// `range` seconds up to `now`, in buckets: average traffic per second, and the most
    /// people connected and in voice. Buckets with no points are left out.
    pub fn series(&self, range: i64, now: i64) -> StatsHistory {
        let step = (range / BUCKETS).max(STEP.as_secs() as i64);
        let start = now - range;
        let buckets = (range / step) as usize;
        let mut sums = vec![Sum::default(); buckets];
        let points = self.0.lock().expect("stats history");
        let bucket = |at: i64| {
            let index = (at - start - 1).max(0) / step;
            (at > start && at <= now).then_some((index as usize).min(buckets - 1))
        };
        for (before, point) in points.iter().zip(points.iter().skip(1)) {
            let Some(index) = bucket(point.at) else {
                continue;
            };
            let sum = &mut sums[index];
            sum.secs += (point.at - before.at).max(0) as u64;
            sum.received += point.control.0.saturating_sub(before.control.0);
            sum.sent += point.control.1.saturating_sub(before.control.1);
            // LiveKit restarted, or didn't answer one of the two times: no media for this step.
            if let (Some(now), Some(then)) = (point.media, before.media)
                && now.0 >= then.0
                && now.1 >= then.1
            {
                sum.received += now.0 - then.0;
                sum.sent += now.1 - then.1;
            }
        }
        for point in points.iter() {
            let Some(index) = bucket(point.at) else {
                continue;
            };
            let sum = &mut sums[index];
            sum.seen = true;
            sum.connected = sum.connected.max(point.connected);
            if let Some(voice) = point.voice {
                sum.voice = Some(sum.voice.unwrap_or(0).max(voice));
            }
        }
        drop(points);
        let points = (sums.into_iter().enumerate())
            .filter(|(_, sum)| sum.seen)
            .map(|(index, sum)| {
                let per_sec = |bytes: u64| bytes.checked_div(sum.secs);
                StatsPoint {
                    at: start + (index as i64 + 1) * step,
                    received_per_sec: per_sec(sum.received),
                    sent_per_sec: per_sec(sum.sent),
                    connected: sum.connected,
                    voice: sum.voice,
                }
            })
            .collect();
        StatsHistory {
            range,
            step,
            points,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Sum {
    seen: bool,
    secs: u64,
    received: u64,
    sent: u64,
    connected: u64,
    voice: Option<u64>,
}

/// Take a point every `STEP` for as long as the server runs.
pub async fn record(app: App) {
    let mut ticks = tokio::time::interval(STEP);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let (voice, media) = match &app.voice {
            Some(livekit) => livekit.usage().await,
            None => (None, None),
        };
        let stats = &app.stats;
        stats.history.push(Point {
            at: store::now().unwrap_or_default(),
            control: stats.totals(),
            media: media.map(|media| (media.received, media.sent)),
            connected: app.presence.connected() as u64,
            voice: voice.map(|voice| voice.people),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(at: i64, control: u64, media: Option<u64>, connected: u64) -> Point {
        Point {
            at,
            control: (control, control * 2),
            media: media.map(|bytes| (bytes, bytes)),
            connected,
            voice: media.map(|_| connected / 2),
        }
    }

    #[test]
    fn a_range_is_cut_into_buckets_of_rates_and_peaks() {
        let history = History::default();
        // Ten seconds apart, 1000 control bytes and 500 media bytes more each time.
        for step in 0..=30 {
            let at = 1000 + step * 10;
            history.push(point(
                at,
                step as u64 * 1000,
                Some(step as u64 * 500),
                step as u64 % 7,
            ));
        }
        let series = history.series(300, 1300);
        assert_eq!((series.range, series.step), (300, 10));
        assert_eq!(series.points.len(), 30);
        let first = series.points[0];
        assert_eq!(first.at, 1010);
        assert_eq!(first.received_per_sec, Some(150));
        assert_eq!(first.sent_per_sec, Some(250));
        assert_eq!(series.points.last().unwrap().at, 1300);

        // An hour: 60-second buckets, the most people seen in each.
        let series = history.series(3600, 1300);
        assert_eq!(series.step, 60);
        let busiest = series.points.iter().map(|p| p.connected).max();
        assert_eq!(busiest, Some(6));
        // The bucket holding only the very first point has nothing to compare with.
        let rates: Vec<_> = series.points.iter().map(|p| p.received_per_sec).collect();
        assert_eq!(rates[0], None);
        assert!(rates[1..].iter().all(|rate| *rate == Some(150)));
    }

    #[test]
    fn a_livekit_restart_or_gap_adds_no_media() {
        let history = History::default();
        history.push(point(10, 0, Some(9000), 1));
        // LiveKit restarted: its totals went down.
        history.push(point(20, 100, Some(50), 1));
        // LiveKit didn't answer.
        history.push(point(30, 200, None, 1));
        history.push(point(40, 300, Some(80), 1));
        let series = history.series(300, 40);
        let rates: Vec<_> = series.points.iter().map(|p| p.received_per_sec).collect();
        assert_eq!(rates, [None, Some(10), Some(10), Some(10)]);
        // In voice is unknown while LiveKit didn't answer.
        assert_eq!(
            series.points.iter().find(|p| p.at == 30).unwrap().voice,
            None
        );
    }

    #[test]
    fn old_points_drop_off_after_a_day() {
        let history = History::default();
        for at in 0..KEEP as i64 + 5 {
            history.push(point(at * 10, 0, None, 1));
        }
        let points = history.0.lock().unwrap();
        assert_eq!(points.len(), KEEP);
        assert_eq!(points.front().unwrap().at, 50);
    }

    #[test]
    fn an_empty_or_young_history_has_few_points() {
        let history = History::default();
        assert!(history.series(86_400, 100_000).points.is_empty());
        history.push(point(99_995, 0, None, 3));
        let series = history.series(86_400, 100_000);
        assert_eq!(series.step, 1440);
        assert_eq!(series.points.len(), 1);
        assert_eq!(series.points[0].connected, 3);
        assert_eq!(series.points[0].received_per_sec, None);
    }
}
