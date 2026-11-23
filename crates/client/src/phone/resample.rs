//! Between the call's 48 kHz and the phone's 8 kHz: a low-pass filter at the phone
//! band's edge, then every sixth sample (down), or five zeros after each (up).

use std::{collections::VecDeque, sync::Arc};

/// 48 kHz samples per 8 kHz sample.
pub const RATIO: usize = 6;
const TAPS: usize = 96;
/// Just below 4 kHz, half the phone's rate, so nothing folds back into the band.
const CUTOFF_HZ: f64 = 3600.0;

/// A windowed-sinc low-pass at 48 kHz (Blackman window), summing to 1.
fn taps() -> Arc<[f32]> {
    let fc = CUTOFF_HZ / 48_000.0;
    let middle = (TAPS - 1) as f64 / 2.0;
    let raw: Vec<f64> = (0..TAPS)
        .map(|n| {
            let x = n as f64 - middle;
            let sinc = if x == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x)
            };
            let phase = 2.0 * std::f64::consts::PI * n as f64 / (TAPS - 1) as f64;
            sinc * (0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos())
        })
        .collect();
    let sum: f64 = raw.iter().sum();
    raw.iter().map(|tap| (tap / sum) as f32).collect()
}

struct Filter {
    taps: Arc<[f32]>,
    history: VecDeque<f32>,
}

impl Filter {
    fn new() -> Self {
        Self {
            taps: taps(),
            history: VecDeque::from(vec![0.0; TAPS]),
        }
    }

    fn push(&mut self, sample: f32) {
        self.history.pop_front();
        self.history.push_back(sample);
    }

    fn output(&self) -> f32 {
        self.history
            .iter()
            .zip(self.taps.iter())
            .map(|(s, t)| s * t)
            .sum()
    }
}

fn clamp(sample: f32) -> i16 {
    sample.round().clamp(-32768.0, 32767.0) as i16
}

pub struct Down {
    filter: Filter,
}

impl Down {
    pub fn new() -> Self {
        Self {
            filter: Filter::new(),
        }
    }

    /// `input.len()` must be a multiple of `RATIO`.
    pub fn run(&mut self, input: &[i16]) -> Vec<i16> {
        input
            .as_chunks::<RATIO>()
            .0
            .iter()
            .map(|chunk| {
                for &sample in chunk {
                    self.filter.push(f32::from(sample));
                }
                clamp(self.filter.output())
            })
            .collect()
    }
}

pub struct Up {
    filter: Filter,
}

impl Up {
    pub fn new() -> Self {
        Self {
            filter: Filter::new(),
        }
    }

    pub fn run(&mut self, input: &[i16]) -> Vec<i16> {
        let mut out = Vec::with_capacity(input.len() * RATIO);
        for &sample in input {
            for at in 0..RATIO {
                // The zeros take the energy out; scaling the sample puts it back.
                let value = if at == 0 {
                    f32::from(sample) * RATIO as f32
                } else {
                    0.0
                };
                self.filter.push(value);
                out.push(clamp(self.filter.output()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, rate: f64, len: usize) -> Vec<i16> {
        (0..len)
            .map(|n| (10_000.0 * (2.0 * std::f64::consts::PI * hz * n as f64 / rate).sin()) as i16)
            .collect()
    }

    fn rms(samples: &[i16]) -> f64 {
        let sum: f64 = samples.iter().map(|&s| f64::from(s).powi(2)).sum();
        (sum / samples.len() as f64).sqrt()
    }

    #[test]
    fn speech_passes_and_high_sound_is_removed() {
        let mut down = Down::new();
        let speech = down.run(&tone(1000.0, 48_000.0, 48_000));
        assert_eq!(speech.len(), 8000);
        // Past the filter's start-up, 1 kHz keeps its level (10000 / √2).
        assert!((rms(&speech[200..]) - 7071.0).abs() < 150.0);
        let mut down = Down::new();
        let high = down.run(&tone(6000.0, 48_000.0, 48_000));
        assert!(rms(&high[200..]) < 100.0, "6 kHz would fold back to 2 kHz");
        let mut up = Up::new();
        let back = up.run(&tone(1000.0, 8000.0, 8000));
        assert_eq!(back.len(), 48_000);
        assert!((rms(&back[1200..]) - 7071.0).abs() < 200.0);
    }
}
