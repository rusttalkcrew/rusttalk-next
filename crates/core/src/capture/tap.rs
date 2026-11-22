//! The platform-independent half of macOS app audio, kept apart so Linux CI tests it.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use super::{App, FRAME_SAMPLES, SAMPLE_RATE};

/// A process known to Core Audio.
pub(super) struct Process {
    pub pid: i32,
    /// Its Core Audio object, which a tap names instead of the PID.
    pub object: u32,
    pub playing: bool,
    pub name: String,
}

/// The processes playing sound now, once each, never RustTalk itself: sharing our
/// own playback would echo the call.
pub(super) fn playing(processes: impl IntoIterator<Item = Process>, own: i32) -> Vec<App> {
    let mut apps = BTreeMap::new();
    for process in processes {
        if process.playing && process.pid > 0 && process.pid != own {
            apps.entry(process.pid).or_insert_with(|| App {
                // The object too, so a recycled PID can't redirect a stale pick.
                id: format!("{}:{}", process.pid, process.object),
                name: process.name,
            });
        }
    }
    apps.into_values().collect()
}

/// The PID and Core Audio object in an ID from `playing`.
pub(super) fn parse_id(id: &str) -> Result<(i32, u32)> {
    let (pid, object) = id.split_once(':').context("invalid audio process ID")?;
    Ok((
        pid.parse().context("invalid audio process ID")?,
        object.parse().context("invalid audio process ID")?,
    ))
}

/// Interleaved samples as stereo frames: mono plays on both sides, channels past
/// the first two are dropped.
pub(super) fn stereo(samples: &[f32], channels: usize) -> impl Iterator<Item = [f32; 2]> + '_ {
    let right = channels.min(2).saturating_sub(1);
    samples
        .chunks_exact(channels.max(1))
        .map(move |frame| [frame[0], frame[right]])
}

/// One buffer per channel as stereo frames; a single buffer plays on both sides.
pub(super) fn planar<'a>(buffers: &[&'a [f32]]) -> impl Iterator<Item = [f32; 2]> + 'a {
    let left = buffers.first().copied().unwrap_or_default();
    let right = buffers.get(1).copied().unwrap_or(left);
    left.iter().zip(right).map(|(&left, &right)| [left, right])
}

/// The bundle ID and each shorter parent, down to two parts: a browser's
/// `com.google.Chrome.helper` plays the sound, but people know `com.google.Chrome`.
pub(super) fn bundle_prefixes(id: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(id), |id| id.rsplit_once('.').map(|(parent, _)| parent))
        .take_while(|id| id.contains('.'))
}

/// About three seconds of frames.
const SILENT_FRAMES: usize = 300;

/// Watches the first seconds of a capture: macOS delivers silence, not an error, when
/// System Audio Recording is off. It answers once, so a later pause never ends a share.
#[derive(Default)]
pub(super) struct SilenceCheck {
    frames: usize,
    heard: bool,
}

impl SilenceCheck {
    /// After the first three seconds: whether every sample so far was zero. Then never again.
    pub fn feed(&mut self, frame: &[i16]) -> Option<bool> {
        if self.frames > SILENT_FRAMES {
            return None;
        }
        self.heard |= frame.iter().any(|&sample| sample != 0);
        self.frames += 1;
        (self.frames > SILENT_FRAMES).then_some(!self.heard)
    }
}

/// Turns float stereo at the output device's rate into the 48 kHz s16 10 ms frames
/// the other platforms deliver. Linear interpolation is plenty for shared app sound.
pub(super) struct Converter {
    /// Input frames per output frame.
    step: f64,
    /// Where the next output frame lies between `last` (0) and the next input (1).
    at: f64,
    last: [f32; 2],
    frame: [i16; FRAME_SAMPLES],
    filled: usize,
}

impl Converter {
    pub fn new(rate: f64) -> Self {
        Self {
            step: rate / f64::from(SAMPLE_RATE),
            at: 0.0,
            last: [0.0; 2],
            frame: [0; FRAME_SAMPLES],
            filled: 0,
        }
    }

    pub fn push(
        &mut self,
        input: impl IntoIterator<Item = [f32; 2]>,
        mut emit: impl FnMut(&[i16; FRAME_SAMPLES]),
    ) {
        for next in input {
            while self.at < 1.0 {
                let t = self.at as f32;
                for (last, next) in self.last.iter().zip(next) {
                    let sample = last + (next - last) * t;
                    self.frame[self.filled] =
                        (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
                    self.filled += 1;
                }
                if self.filled == FRAME_SAMPLES {
                    emit(&self.frame);
                    self.filled = 0;
                }
                self.at += self.step;
            }
            self.at -= 1.0;
            self.last = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: i32, object: u32, playing: bool, name: &str) -> Process {
        Process {
            pid,
            object,
            playing,
            name: name.into(),
        }
    }

    #[test]
    fn lists_other_playing_processes_once() {
        let apps = playing(
            [
                process(30, 7, true, "Music"),
                process(10, 3, true, "RustTalk"),
                process(20, 5, false, "Mail"),
                process(30, 8, true, "Music again"),
                process(0, 1, true, "kernel"),
                process(25, 6, true, "Safari"),
            ],
            10,
        );
        let expected = [("25:6", "Safari"), ("30:7", "Music")];
        assert_eq!(apps.len(), expected.len());
        for (app, (id, name)) in apps.iter().zip(expected) {
            assert_eq!((app.id.as_str(), app.name.as_str()), (id, name));
        }
    }

    #[test]
    fn parses_ids() {
        assert_eq!(parse_id("123:45").unwrap(), (123, 45));
        assert!(parse_id("123").is_err());
        assert!(parse_id("x:1").is_err());
        assert!(parse_id(super::super::SYSTEM).is_err());
    }

    #[test]
    fn stereo_duplicates_mono_and_drops_extra_channels() {
        let mono: Vec<_> = stereo(&[0.1, 0.2], 1).collect();
        assert_eq!(mono, [[0.1, 0.1], [0.2, 0.2]]);
        let surround: Vec<_> = stereo(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 3).collect();
        assert_eq!(surround, [[1.0, 2.0], [4.0, 5.0]]);
        assert_eq!(stereo(&[1.0], 0).count(), 1);
    }

    #[test]
    fn planar_pairs_channel_buffers() {
        let (left, right) = ([1.0, 2.0], [3.0, 4.0]);
        let both: Vec<_> = planar(&[&left, &right]).collect();
        assert_eq!(both, [[1.0, 3.0], [2.0, 4.0]]);
        let mono: Vec<_> = planar(&[&left]).collect();
        assert_eq!(mono, [[1.0, 1.0], [2.0, 2.0]]);
        assert_eq!(planar(&[]).count(), 0);
    }

    #[test]
    fn helpers_fall_back_to_their_app() {
        let ids: Vec<_> = bundle_prefixes("com.google.Chrome.helper.renderer").collect();
        assert_eq!(
            ids,
            [
                "com.google.Chrome.helper.renderer",
                "com.google.Chrome.helper",
                "com.google.Chrome",
                "com.google"
            ]
        );
        assert_eq!(bundle_prefixes("Music").count(), 0);
    }

    #[test]
    fn silence_check_answers_once() {
        let mut check = SilenceCheck::default();
        let quiet = [0; 4];
        let answers: Vec<_> = (0..SILENT_FRAMES + 50)
            .filter_map(|_| check.feed(&quiet))
            .collect();
        assert_eq!(answers, [true]);

        let mut check = SilenceCheck::default();
        assert_eq!(check.feed(&[0, 1, 0, 0]), None);
        let answers: Vec<_> = (0..SILENT_FRAMES)
            .filter_map(|_| check.feed(&quiet))
            .collect();
        assert_eq!(answers, [false]);
    }

    fn convert(rate: f64, input: &[[f32; 2]]) -> Vec<i16> {
        let mut converter = Converter::new(rate);
        let mut out = Vec::new();
        converter.push(input.iter().copied(), |frame| {
            assert_eq!(frame.len(), FRAME_SAMPLES);
            out.extend_from_slice(frame);
        });
        out
    }

    #[test]
    fn passes_48k_through_in_10ms_frames() {
        let input: Vec<_> = (0..960).map(|i| [i as f32 / 1000.0, -0.5]).collect();
        let out = convert(48_000.0, &input);
        assert_eq!(out.len(), 2 * FRAME_SAMPLES);
        // One frame of delay: the first output is the silence before the input.
        assert_eq!(&out[..4], [0, 0, 0, -16384]);
        assert_eq!(out[2 * 480], (0.479 * 32767.0f32).round() as i16);
    }

    #[test]
    fn resamples_44k1_to_48k() {
        // One second in gives one second (100 frames) out, give or take a frame.
        let input = vec![[0.25, 0.25]; 44_100];
        let out = convert(44_100.0, &input);
        assert!((99..=100).contains(&(out.len() / FRAME_SAMPLES)));
        assert!(out[100..].iter().all(|&s| s == 8192));
    }

    #[test]
    fn clamps_loud_input() {
        let out = convert(48_000.0, &[[2.0, -2.0]; 481]);
        assert_eq!(&out[2..4], [i16::MAX, -i16::MAX]);
    }
}
