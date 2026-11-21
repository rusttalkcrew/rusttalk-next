//! Notices a noisy or too loud microphone during a call, from levels measured on this
//! device. Pure: 10 ms frames in, at most one verdict per call out. Nothing leaves the
//! device; the UI turns the verdict into a hint.

/// 10 ms frames per second.
const FRAMES_PER_SECOND: u32 = 100;
/// How long the background noise must stay high, without a pause.
const NOISY_FRAMES: u32 = 20 * FRAMES_PER_SECOND;
/// Background noise above this, in dBFS after processing, is loud enough to hear in a
/// call; a quiet room measures below -60 and the automatic voice level tops out at -35.
pub const NOISY_DB: f32 = -45.0;
/// Clipping is judged over windows of this many frames.
const WINDOW_FRAMES: u32 = 20 * FRAMES_PER_SECOND;
/// Half a second of clipped frames in one window: a shout is a few frames, a mic that
/// is turned up too far clips on every word.
const CLIPPED_FRAMES: u32 = 50;
/// A sample this close to full scale counts as clipped.
pub const CLIP_LEVEL: u16 = 32_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Steady background noise between speech.
    Noisy,
    /// The signal runs into the top of the range.
    Loud,
}

/// What one 10 ms frame says about the microphone.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    /// The background noise estimate in dBFS (a low percentile of the last seconds,
    /// which speech doesn't lift).
    pub floor: f32,
    /// A sample reached `CLIP_LEVEL` before any gain was applied.
    pub clipped: bool,
    /// Nothing is sent while muted, so nothing is judged.
    pub muted: bool,
}

#[derive(Default)]
pub struct Detector {
    noisy: u32,
    window: u32,
    clipped: u32,
    done: bool,
}

impl Detector {
    /// Judge one frame. Returns a verdict at most once; `Detector::default()` again
    /// for the next call.
    pub fn push(&mut self, frame: Frame) -> Option<Verdict> {
        if self.done {
            return None;
        }
        if frame.muted {
            // Starts over: a sustained problem means unmuted the whole time.
            *self = Self::default();
            return None;
        }
        self.noisy = if frame.floor > NOISY_DB {
            self.noisy + 1
        } else {
            0
        };
        self.clipped += u32::from(frame.clipped);
        self.window += 1;
        let verdict = if self.clipped >= CLIPPED_FRAMES {
            Some(Verdict::Loud)
        } else if self.noisy >= NOISY_FRAMES {
            Some(Verdict::Noisy)
        } else {
            None
        };
        if self.window >= WINDOW_FRAMES {
            self.window = 0;
            self.clipped = 0;
        }
        self.done = verdict.is_some();
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: f32 = -70.0;

    fn frame(floor: f32, clipped: bool) -> Frame {
        Frame {
            floor,
            clipped,
            muted: false,
        }
    }

    /// Feeds `seconds` of identical frames; the first verdict, if any.
    fn feed(detector: &mut Detector, seconds: u32, frame: Frame) -> Option<Verdict> {
        (0..seconds * FRAMES_PER_SECOND).find_map(|_| detector.push(frame))
    }

    #[test]
    fn quiet_room_with_speech_gives_no_hint() {
        let mut detector = Detector::default();
        for _ in 0..10 {
            // Words clip nothing and leave the floor low.
            assert_eq!(feed(&mut detector, 5, frame(QUIET, false)), None);
            assert_eq!(feed(&mut detector, 5, frame(-62.0, false)), None);
        }
    }

    #[test]
    fn steady_noise_floor_gives_the_noise_hint_after_twenty_seconds() {
        let mut detector = Detector::default();
        assert_eq!(feed(&mut detector, 19, frame(-40.0, false)), None);
        assert_eq!(
            feed(&mut detector, 2, frame(-40.0, false)),
            Some(Verdict::Noisy)
        );
    }

    #[test]
    fn noise_that_comes_and_goes_gives_no_hint() {
        let mut detector = Detector::default();
        for _ in 0..5 {
            assert_eq!(feed(&mut detector, 15, frame(-40.0, false)), None);
            assert_eq!(feed(&mut detector, 1, frame(QUIET, false)), None);
        }
    }

    #[test]
    fn clipping_gives_the_loud_hint() {
        let mut detector = Detector::default();
        // A few clipped frames, like one shout, are fine.
        for _ in 0..10 {
            detector.push(frame(QUIET, true));
        }
        assert_eq!(feed(&mut detector, 5, frame(QUIET, false)), None);
        let verdict = (0..CLIPPED_FRAMES).find_map(|_| detector.push(frame(QUIET, true)));
        assert_eq!(verdict, Some(Verdict::Loud));
    }

    #[test]
    fn clipping_spread_over_a_long_time_is_forgiven() {
        let mut detector = Detector::default();
        // 10 clipped frames per 20 s window never add up.
        for _ in 0..10 {
            for _ in 0..10 {
                assert_eq!(detector.push(frame(QUIET, true)), None);
            }
            assert_eq!(feed(&mut detector, 20, frame(QUIET, false)), None);
        }
    }

    #[test]
    fn muted_means_nothing_and_starts_over() {
        let mut detector = Detector::default();
        let muted = Frame {
            muted: true,
            ..frame(-40.0, true)
        };
        assert_eq!(feed(&mut detector, 60, muted), None);
        // Noise that lasted 19 s before muting doesn't carry over.
        assert_eq!(feed(&mut detector, 19, frame(-40.0, false)), None);
        assert_eq!(feed(&mut detector, 5, muted), None);
        assert_eq!(feed(&mut detector, 19, frame(-40.0, false)), None);
    }

    #[test]
    fn at_most_one_verdict_per_call() {
        let mut detector = Detector::default();
        assert_eq!(
            feed(&mut detector, 25, frame(-40.0, false)),
            Some(Verdict::Noisy)
        );
        assert_eq!(feed(&mut detector, 60, frame(-40.0, true)), None);
        // A new call starts fresh.
        detector = Detector::default();
        assert_eq!(
            feed(&mut detector, 25, frame(-40.0, false)),
            Some(Verdict::Noisy)
        );
    }
}
