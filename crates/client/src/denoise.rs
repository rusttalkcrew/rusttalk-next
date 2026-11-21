//! AI noise removal: DeepFilterNet3 run on this device with tract (pure Rust, no
//! native libraries). Its weights are downloaded only when the person asks for them,
//! from a pinned upstream commit, and checked against a pinned SHA-256 before every load.

use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use deep_filter_rt::tract::{DfParams, DfTract, RuntimeParams};
use flate2::read::GzDecoder;
use ndarray::Array2;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::audio::{FRAME, RATE};

/// Upstream's DeepFilterNet3 model at a fixed commit (MIT or Apache-2.0, like its code).
const URL: &str = "https://github.com/Rikorose/DeepFilterNet/raw/d375b2d8309e0935d165700c91da9de862a99c31/models/DeepFilterNet3_onnx.tar.gz";
const SHA256: &str = "c94d91f70911001c946e0fabb4aa9adc37045f45a03b56008cb0c8244cb63616";
pub const SIZE: u64 = 7_983_136;

/// Where the downloaded model lives in the app data directory.
pub fn path(data: &Path) -> PathBuf {
    data.join("deepfilternet3.tar.gz")
}

fn verify(bytes: &[u8]) -> Result<()> {
    check(bytes, SIZE, SHA256)
}

fn check(bytes: &[u8], size: u64, sha256: &str) -> Result<()> {
    ensure!(
        bytes.len() as u64 == size && hex::encode(Sha256::digest(bytes)) == sha256,
        "noise model checksum mismatch"
    );
    Ok(())
}

/// Downloads and verifies the model, then moves it into place; `progress` gets the bytes so far.
pub async fn download(file: &Path, mut progress: impl FnMut(u64)) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        // GitHub redirects to its raw content host; the pinned hash covers every byte.
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .build()?;
    let mut response = client
        .get(URL)
        .send()
        .await
        .map_err(reqwest::Error::without_url)?;
    ensure!(
        response.status() == reqwest::StatusCode::OK,
        "model host returned HTTP {}",
        response.status()
    );
    let mut bytes = Vec::with_capacity(SIZE as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(reqwest::Error::without_url)?
    {
        ensure!(
            (bytes.len() + chunk.len()) as u64 <= SIZE,
            "noise model too large"
        );
        bytes.extend_from_slice(&chunk);
        progress(bytes.len() as u64);
    }
    verify(&bytes)?;
    let mut temp = NamedTempFile::new_in(file.parent().context("model directory")?)?;
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(file)?;
    Ok(())
}

/// Reads and verifies the downloaded model, then builds it.
pub fn load(file: &Path) -> Result<Denoiser> {
    let bytes = fs::read(file).context("read noise model")?;
    verify(&bytes)?;
    let mut files = HashMap::new();
    for entry in tar::Archive::new(GzDecoder::new(&bytes[..])).entries()? {
        let mut entry = entry?;
        let name = entry.path()?.file_name().map(|n| n.to_owned());
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        if let Some(name) = name {
            files.insert(name.to_string_lossy().into_owned(), data);
        }
    }
    let mut take = |name: &str| {
        files
            .remove(name)
            .with_context(|| format!("{name} missing from the noise model"))
    };
    let params = DfParams::from_bytes(
        &take("config.ini")?,
        take("enc.onnx")?,
        take("erb_dec.onnx")?,
        take("df_dec.onnx")?,
    )?;
    Denoiser::new(DfTract::new(params, &RuntimeParams::default())?)
}

pub struct Denoiser {
    model: DfTract,
    noisy: Array2<f32>,
    clean: Array2<f32>,
}

impl Denoiser {
    fn new(model: DfTract) -> Result<Self> {
        // The pipeline hands over 10 ms frames at 48 kHz, DeepFilterNet3's own hop and rate.
        ensure!(
            model.hop_size == FRAME && model.sr == RATE as usize && model.ch == 1,
            "unexpected noise model format"
        );
        Ok(Self {
            model,
            noisy: Array2::zeros((1, FRAME)),
            clean: Array2::zeros((1, FRAME)),
        })
    }

    /// Cleans one 10 ms frame in place; the output trails the input by 30 ms.
    pub fn process(&mut self, frame: &mut [i16]) -> Result<()> {
        for (noisy, &sample) in self.noisy.iter_mut().zip(frame.iter()) {
            *noisy = f32::from(sample) / 32768.0;
        }
        self.model
            .process(self.noisy.view(), self.clean.view_mut())?;
        ensure!(
            self.clean.iter().all(|x| x.is_finite()),
            "noise model output is not finite"
        );
        for (sample, &clean) in frame.iter_mut().zip(self.clean.iter()) {
            *sample = (clean * 32768.0).clamp(-32768.0, 32767.0) as i16;
        }
        Ok(())
    }
}

type Samples = [i16; FRAME];
const MODEL_DELAY: usize = 3;
const DEADLINE: Duration = Duration::from_millis(8);
const QUEUE: usize = 16;

struct Job {
    sequence: u64,
    deadline: Instant,
    strength: u8,
    samples: Samples,
}

pub struct Worker {
    input: rtrb::Producer<Job>,
    output: rtrb::Consumer<(u64, Option<Samples>)>,
    thread: std::thread::Thread,
    sequence: u64,
    misses: usize,
}

impl Worker {
    pub fn start(mut model: Denoiser) -> Result<Self> {
        let mut strength = 0;
        Self::spawn(move |samples, db| {
            if db != strength {
                model.model.set_atten_lim(f32::from(db));
                strength = db;
            }
            model.process(samples)
        })
    }

    fn spawn(
        mut infer: impl FnMut(&mut Samples, u8) -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let (input, mut jobs) = rtrb::RingBuffer::<Job>::new(QUEUE);
        let (mut results, output) = rtrb::RingBuffer::new(QUEUE);
        let mic = std::thread::current();
        let worker = std::thread::Builder::new()
            .name("ai-denoise".into())
            .spawn(move || {
                loop {
                    let Ok(mut job) = jobs.pop() else {
                        if jobs.is_abandoned() {
                            break;
                        }
                        std::thread::park_timeout(Duration::from_millis(50));
                        continue;
                    };
                    if let Err(error) = infer(&mut job.samples, job.strength) {
                        tracing::warn!(%error, "noise inference failed");
                        break;
                    }
                    let clean = (Instant::now() <= job.deadline).then_some(job.samples);
                    if results.push((job.sequence, clean)).is_err() {
                        break;
                    }
                    mic.unpark();
                }
            })?;
        Ok(Self {
            input,
            output,
            thread: worker.thread().clone(),
            sequence: 0,
            misses: 0,
        })
    }

    pub fn overloaded(&self) -> bool {
        self.misses >= 3 || self.input.is_abandoned()
    }

    /// Called on the mic processing thread. Jobs in the same device callback each
    /// get their own wall-clock budget, even when their samples arrive in a burst.
    pub fn process(&mut self, samples: &Samples, strength: u8) -> Option<Samples> {
        let sequence = self.sequence;
        self.sequence += 1;
        let deadline = Instant::now() + DEADLINE;
        if self
            .input
            .push(Job {
                sequence,
                deadline,
                strength: strength.clamp(6, 48),
                samples: *samples,
            })
            .is_err()
        {
            self.misses = 3;
            return None;
        }
        self.thread.unpark();
        let clean = loop {
            if let Ok((seq, clean)) = self.output.pop() {
                if seq == sequence {
                    break clean;
                }
                continue;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.input.is_abandoned() {
                break None;
            }
            std::thread::park_timeout(left);
        };
        if clean.is_some() {
            self.misses = 0;
        } else {
            self.misses += 1;
        }
        clean.filter(|_| sequence >= MODEL_DELAY as u64)
    }
}

#[derive(Clone, Copy, PartialEq, Default)]
enum PathKind {
    #[default]
    Live,
    Dry,
    Clean,
}

/// Keep one processed hop in reserve: on a miss, its clean and time-aligned
/// dry versions are both available for a full fade before the missing hop plays.
pub struct Mix {
    dry: [Samples; MODEL_DELAY + 1],
    next: usize,
    pending: Option<Samples>,
    path: PathKind,
}

impl Default for Mix {
    fn default() -> Self {
        Self {
            dry: [[0; FRAME]; MODEL_DELAY + 1],
            next: 0,
            pending: None,
            path: PathKind::Live,
        }
    }
}

impl Mix {
    pub fn apply(&mut self, live: &mut Samples, clean: Option<Samples>, enabled: bool) {
        let delayed = std::mem::replace(&mut self.dry[self.next], *live);
        self.next = (self.next + 1) % self.dry.len();
        let ready = clean.is_some();
        let previous = std::mem::replace(&mut self.pending, clean);
        let target = if enabled && ready && previous.is_some() {
            PathKind::Clean
        } else if enabled && self.path != PathKind::Live {
            PathKind::Dry
        } else {
            PathKind::Live
        };
        let signal = |path| match path {
            PathKind::Clean => previous.as_ref().expect("previous clean frame"),
            PathKind::Dry => &delayed,
            PathKind::Live => &*live,
        };
        let from = signal(self.path);
        let to = signal(target);
        *live = if self.path == target {
            *to
        } else {
            std::array::from_fn(|i| {
                let weight = i as f32 / (FRAME - 1) as f32;
                (f32::from(from[i]) * (1.0 - weight) + f32::from(to[i]) * weight).round() as i16
            })
        };
        self.path = target;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn bursts_of_four_and_five_keep_processed_output() {
        let mut worker = Worker::spawn(|samples, _| {
            std::thread::sleep(Duration::from_micros(250));
            samples[0] += 1;
            Ok(())
        })
        .unwrap();
        let mut seq = 0;
        for burst in [4, 5, 4, 5] {
            for _ in 0..burst {
                let result = worker.process(&[seq; FRAME], 24);
                if seq >= MODEL_DELAY as i16 {
                    assert_eq!(result.unwrap()[0], seq + 1);
                }
                assert!(!worker.overloaded());
                seq += 1;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    #[test]
    fn persistent_deadline_misses_request_a_fresh_worker() {
        let mut worker = Worker::spawn(|_, _| {
            std::thread::sleep(DEADLINE * 4);
            Ok(())
        })
        .unwrap();
        for misses in 1..=3 {
            assert!(worker.process(&[1; FRAME], 24).is_none());
            assert_eq!(worker.overloaded(), misses == 3);
        }
    }

    #[test]
    fn missed_hops_fade_to_aligned_dry_and_back() {
        let mut mix = Mix::default();
        for i in 0..12 {
            let mut samples = [100 + i; FRAME];
            let clean = (i >= 3 && i != 6 && i != 10).then_some([500 + i; FRAME]);
            mix.apply(&mut samples, clean, i < 10);
            let endpoints = match i {
                4 => (104, 503), // live to clean
                6 => (505, 102), // clean to aligned dry
                7 => (103, 103),
                8 => (104, 507),
                10 => (509, 110), // disabled mid-call
                _ => continue,
            };
            assert_eq!((samples[0], samples[FRAME - 1]), endpoints);
        }
    }

    #[test]
    fn only_the_pinned_model_passes() {
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(check(b"abc", 3, abc).is_ok());
        assert!(check(b"abd", 3, abc).is_err());
        assert!(check(b"abc", 4, abc).is_err());
        let dir = tempfile::tempdir().unwrap();
        let file = path(dir.path());
        // Right size, wrong bytes.
        fs::write(&file, vec![0; SIZE as usize]).unwrap();
        let error = load(&file).err().unwrap().to_string();
        assert!(error.contains("checksum"), "{error}");
    }

    /// The real model, from `RUSTTALK_NOISE_MODEL` (a downloaded copy) or the pinned URL
    /// with `RUSTTALK_NOISE_MODEL=download`; skipped without it, as tests stay offline.
    fn model() -> Option<Denoiser> {
        let from = std::env::var("RUSTTALK_NOISE_MODEL").ok()?;
        let dir = tempfile::tempdir().unwrap();
        let file = if from == "download" {
            let file = path(dir.path());
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(download(&file, |_| {})).unwrap();
            file
        } else {
            PathBuf::from(from)
        };
        let started = Instant::now();
        let model = load(&file).unwrap();
        eprintln!("loaded in {:?}", started.elapsed());
        Some(model)
    }

    #[test]
    fn removes_noise_from_a_noisy_tone() {
        let Some(mut denoiser) = model() else {
            eprintln!("set RUSTTALK_NOISE_MODEL to run this test");
            return;
        };
        // A speech-like tone with harmonics for 3 s, in white noise.
        let mut seed = 1u32;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32 - 0.5) * 0.2
        };
        let frames = 300;
        let (mut noise_in, mut noise_out, mut tone_in, mut tone_out) = (0.0, 0.0, 0.0, 0.0);
        let mut slowest = Duration::ZERO;
        denoiser.model.set_atten_lim(24.0);
        let mut elapsed = Duration::ZERO;
        let mut missed = 0;
        for i in 0..frames {
            let speaking = i % 100 < 50;
            let tone: Vec<f32> = (0..FRAME)
                .map(|n| {
                    let t = (i * FRAME + n) as f32 / RATE as f32;
                    let tone: f32 = (1..4)
                        .map(|h| (std::f32::consts::TAU * 220.0 * h as f32 * t).sin() * 0.1)
                        .sum();
                    if speaking { tone } else { 0.0 }
                })
                .collect();
            let mut frame: Vec<i16> = tone
                .iter()
                .map(|t| ((noise() + t) * 32768.0) as i16)
                .collect();
            let energy = |f: &[i16]| f.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
            let before = energy(&frame);
            let took = Instant::now();
            denoiser.process(&mut frame).unwrap();
            let took = took.elapsed();
            elapsed += took;
            slowest = slowest.max(took);
            missed += usize::from(took > Duration::from_millis(10));
            // Noise-only frames after the model settled and its 30 ms delay passed.
            if !speaking && i % 100 > 55 && i > 100 {
                noise_in += before;
                noise_out += energy(&frame);
            }
            if speaking && i % 100 > 10 && i > 100 {
                tone_in += tone
                    .iter()
                    .map(|&t| f64::from(t * 32768.0).powi(2))
                    .sum::<f64>();
                tone_out += energy(&frame);
            }
        }
        let per_frame = elapsed / frames as u32;
        let reduced = 10.0 * (noise_in / noise_out.max(1.0)).log10();
        let kept = 10.0 * (tone_out / tone_in).log10();
        eprintln!(
            "noise {reduced:.1} dB down, tone {kept:+.1} dB, {per_frame:?} per 10 ms frame, slowest {slowest:?}, {missed}/{frames} over deadline"
        );
        assert!(reduced > 10.0, "noise only {reduced:.1} dB down");
        assert!(kept > -6.0, "tone {kept:.1} dB down");
    }
}
