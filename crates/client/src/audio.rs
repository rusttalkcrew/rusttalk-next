//! Voice audio on its own threads, so device work never blocks the UI.
//!
//! Microphone: cpal capture → 48 kHz mono → WebRTC audio processing (echo
//! cancellation, noise suppression, auto gain) → push-to-talk or voice gate →
//! the `NativeAudioSource` that voice publishes.
//! Speakers: each remote track is read as 48 kHz mono, scaled per stream (a
//! person's voice or one app they share), mixed and played through cpal; the mix
//! also feeds the echo canceller.

use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    pin::pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed},
        mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender},
    },
    task::{Context, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use cpal::{
    BufferSize, Device, DeviceId, ErrorKind, FromSample, Host, Sample, SampleFormat, SizedSample,
    Stream, StreamConfig, SupportedBufferSize, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures_util::StreamExt;
use livekit::{
    prelude::TrackSid,
    webrtc::{
        audio_frame::AudioFrame,
        audio_source::{AudioSourceOptions, native::NativeAudioSource},
        audio_stream::native::NativeAudioStream,
        native::{apm::AudioProcessingModule, audio_resampler::AudioResampler},
        prelude::RtcAudioTrack,
    },
};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use crate::server::Id;

pub const RATE: u32 = 48_000;
/// 10 ms at 48 kHz mono, the frame size of audio processing and the source.
const FRAME: usize = 480;
/// Quieter than this counts as silence for the meter and speaking rings.
pub const SILENT_DB: f32 = -50.0;
/// Voice activation stays open this long after the last loud frame.
const HOLD_FRAMES: u32 = 40;
/// Playback starts once a track has this much queued, and drops what exceeds the cap.
const PRIME: usize = 3 * FRAME;
const CAP: usize = 20 * FRAME;
const SPEAKING_MS: u64 = 300;
/// Mic thread backlog in messages of up to 10 ms each; audio callbacks drop what doesn't fit.
const MIC_QUEUE: usize = 64;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// cpal device IDs; `None` follows the system default.
    pub input: Option<String>,
    pub output: Option<String>,
    pub push_to_talk: bool,
    /// An `egui::Key` name.
    pub key: String,
    /// Voice activation level in dBFS.
    pub threshold: f32,
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub auto_gain: bool,
    /// Camera name; `None` uses the first camera.
    pub camera: Option<String>,
    pub camera_height: u32,
    pub camera_fps: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            push_to_talk: false,
            key: "V".into(),
            threshold: -42.0,
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain: true,
            camera: None,
            camera_height: 720,
            camera_fps: 30,
        }
    }
}

#[derive(Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
}

/// Cheap to clone; every clone drives the same engine, which stops with the last one.
#[derive(Clone)]
pub struct Audio(Arc<Engine>);

struct Engine {
    shared: Arc<Shared>,
    control: Sender<Control>,
    /// Reads remote tracks; `None` only while dropping.
    runtime: Option<Runtime>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.control.send(Control::Stop);
        // Never wait for track readers on the UI thread.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

#[derive(PartialEq)]
enum Control {
    /// Wanted state or settings changed, or a device failed.
    Refresh,
    Stop,
}

struct Shared {
    file: PathBuf,
    settings: Mutex<Settings>,
    /// Bumped when the processing toggles change, so the mic thread rebuilds its module.
    processing: AtomicU64,
    /// In a voice room: open both devices and feed the source.
    voice: AtomicBool,
    /// Settings are open: open the microphone for the meter.
    testing: AtomicBool,
    /// Push-to-talk key held while the window has focus.
    key_down: AtomicBool,
    muted: AtomicBool,
    deafened: AtomicBool,
    /// Last processed microphone frame, in dBFS (f32 bits).
    level: AtomicU32,
    /// When this person last sent sound, in ms since `start`.
    talked: AtomicU64,
    /// Device latencies for the echo canceller, in ms.
    input_delay: AtomicU32,
    output_delay: AtomicU32,
    /// Frame levels kept for calibration while set.
    record: Mutex<Option<Vec<f32>>>,
    mixer: Mutex<Mixer>,
    devices: Mutex<(Vec<DeviceInfo>, Vec<DeviceInfo>)>,
    /// A device problem the UI shows until the device works again.
    problem: Mutex<Option<String>>,
    source: NativeAudioSource,
    start: Instant,
}

/// A person's voice (`None`) or one app they share, by its track.
pub type StreamId = (Id, Option<TrackSid>);

#[derive(Default)]
struct Mixer {
    tracks: HashMap<String, Track>,
    serial: u64,
    /// Per-stream volume (1.0 is normal) and mute.
    volumes: HashMap<StreamId, (f32, bool)>,
}

struct Track {
    serial: u64,
    stream: StreamId,
    queue: VecDeque<i16>,
    primed: bool,
    loud: u64,
}

type Filter = Box<dyn FnMut(&mut [i16])>;

/// Fixed-size messages, so audio callbacks never allocate.
enum Mic {
    /// The first `len` samples are captured mono at the device rate.
    Near {
        samples: [i16; FRAME],
        len: usize,
        rate: u32,
    },
    /// 10 ms of what the speakers play, at 48 kHz.
    Far([i16; FRAME]),
}

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking audio callback must not take the UI down with it.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }
}

impl Audio {
    /// Starts the device and processing threads; devices open only when needed.
    pub fn new(data: &Path) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("rusttalk-playback")
            .build()
            .context("start playback runtime")?;
        let file = data.join("voice.json");
        let settings = match fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, "voice settings unreadable, using defaults");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        let off = AudioSourceOptions {
            echo_cancellation: false,
            noise_suppression: false,
            auto_gain_control: false,
        };
        let shared = Arc::new(Shared {
            file,
            settings: Mutex::new(settings),
            processing: AtomicU64::new(0),
            voice: AtomicBool::new(false),
            testing: AtomicBool::new(false),
            key_down: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            level: AtomicU32::new((-100f32).to_bits()),
            talked: AtomicU64::new(0),
            input_delay: AtomicU32::new(10),
            output_delay: AtomicU32::new(20),
            record: Mutex::new(None),
            mixer: Mutex::default(),
            devices: Mutex::default(),
            problem: Mutex::new(None),
            // Queue size 0: we hand over exact 10 ms frames, nothing is buffered.
            source: NativeAudioSource::new(off, RATE, 1, 0),
            start: Instant::now(),
        });
        let (control, commands) = mpsc::channel();
        let (mic, frames) = mpsc::sync_channel(MIC_QUEUE);
        {
            let shared = shared.clone();
            let control = control.clone();
            thread::Builder::new()
                .name("rusttalk-audio".into())
                .spawn(move || devices(&shared, &commands, &control, &mic))
                .context("start audio thread")?;
        }
        {
            let shared = shared.clone();
            thread::Builder::new()
                .name("rusttalk-mic".into())
                .spawn(move || process(&shared, &frames))
                .context("start microphone thread")?;
        }
        Ok(Self(Arc::new(Engine {
            shared,
            control,
            runtime: Some(runtime),
        })))
    }

    fn shared(&self) -> &Shared {
        &self.0.shared
    }

    fn refresh(&self) {
        // The device thread runs until the last `Audio` is dropped.
        let _ = self.0.control.send(Control::Refresh);
    }

    /// The source that voice publishes as the microphone track.
    pub fn source(&self) -> NativeAudioSource {
        self.shared().source.clone()
    }

    pub fn settings(&self) -> Settings {
        self.shared().settings()
    }

    pub fn set_settings(&self, settings: Settings) {
        let mut current = lock(&self.shared().settings);
        if *current == settings {
            return;
        }
        let toggles = |s: &Settings| (s.echo_cancellation, s.noise_suppression, s.auto_gain);
        if toggles(&current) != toggles(&settings) {
            self.shared().processing.fetch_add(1, Relaxed);
        }
        *current = settings;
        drop(current);
        self.refresh();
    }

    /// Joined or left a voice room.
    pub fn set_voice(&self, on: bool) {
        self.shared().voice.store(on, Relaxed);
        if !on {
            lock(&self.shared().mixer).tracks.clear();
        }
        self.refresh();
    }

    /// Open the microphone for the settings meter, and list devices again.
    pub fn set_testing(&self, on: bool) {
        if self.shared().testing.swap(on, Relaxed) != on {
            self.refresh();
        }
    }

    pub fn devices(&self) -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
        lock(&self.shared().devices).clone()
    }

    pub fn problem(&self) -> Option<String> {
        lock(&self.shared().problem).clone()
    }

    pub fn set_key_down(&self, down: bool) {
        self.shared().key_down.store(down, Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.shared().muted.load(Relaxed)
    }

    pub fn set_muted(&self, on: bool) {
        self.shared().muted.store(on, Relaxed);
    }

    pub fn deafened(&self) -> bool {
        self.shared().deafened.load(Relaxed)
    }

    pub fn set_deafened(&self, on: bool) {
        self.shared().deafened.store(on, Relaxed);
    }

    /// The last microphone level in dBFS.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared().level.load(Relaxed))
    }

    /// This person is sending sound right now.
    pub fn talking(&self) -> bool {
        self.shared().now() < self.shared().talked.load(Relaxed) + SPEAKING_MS
    }

    /// This person's voice is loud right now; shared apps don't count.
    pub fn speaking(&self, person: &Id) -> bool {
        let now = self.shared().now();
        lock(&self.shared().mixer)
            .tracks
            .values()
            .any(|track| track.stream == (*person, None) && now < track.loud + SPEAKING_MS)
    }

    pub fn volume(&self, stream: &StreamId) -> (f32, bool) {
        let mixer = lock(&self.shared().mixer);
        mixer.volumes.get(stream).copied().unwrap_or((1.0, false))
    }

    pub fn set_volume(&self, stream: StreamId, volume: f32, muted: bool) {
        lock(&self.shared().mixer)
            .volumes
            .insert(stream, (volume, muted));
    }

    /// Start or stop keeping frame levels for calibration; stopping returns them.
    pub fn record(&self, on: bool) -> Option<Vec<f32>> {
        std::mem::replace(&mut *lock(&self.shared().record), on.then(Vec::new))
    }

    /// Play a remote audio track until it ends or is removed.
    pub fn add_track(&self, sid: String, stream: StreamId, track: RtcAudioTrack) {
        let serial = {
            let mut mixer = lock(&self.shared().mixer);
            mixer.serial += 1;
            let serial = mixer.serial;
            // Replaces an older reader of the same track, which then stops.
            mixer.tracks.insert(
                sid.clone(),
                Track {
                    serial,
                    stream,
                    queue: VecDeque::with_capacity(CAP + FRAME),
                    primed: false,
                    loud: 0,
                },
            );
            serial
        };
        let shared = self.0.shared.clone();
        let Some(runtime) = &self.0.runtime else {
            return;
        };
        runtime.spawn(async move {
            let mut stream = NativeAudioStream::new(track, RATE as i32, 1);
            while let Some(frame) = stream.next().await {
                let loud = level_db(&frame.data) > SILENT_DB;
                let now = shared.now();
                let mut mixer = lock(&shared.mixer);
                let Some(track) = mixer.tracks.get_mut(&sid).filter(|t| t.serial == serial) else {
                    return;
                };
                track.queue.extend(frame.data.iter());
                // Backstop after a stall; `mix` corrects normal clock drift.
                if track.queue.len() > CAP {
                    let extra = track.queue.len() - PRIME;
                    track.queue.drain(..extra);
                }
                if loud {
                    track.loud = now;
                }
            }
            let mut mixer = lock(&shared.mixer);
            if mixer.tracks.get(&sid).is_some_and(|t| t.serial == serial) {
                mixer.tracks.remove(&sid);
            }
        });
    }

    pub fn remove_track(&self, sid: &str) {
        lock(&self.shared().mixer).tracks.remove(sid);
    }
}

/// Threshold for voice activation from a quiet and a speaking recording, in dBFS.
pub fn calibrate(quiet: &[f32], speech: &[f32]) -> Option<f32> {
    let noise = percentile(quiet, 0.9)?;
    let voice = percentile(speech, 0.75)?;
    (voice - noise >= 10.0).then(|| noise + (voice - noise) / 3.0)
}

fn percentile(levels: &[f32], at: f32) -> Option<f32> {
    let mut sorted = levels.to_vec();
    sorted.sort_by(f32::total_cmp);
    let index = ((sorted.len() as f32 - 1.0) * at) as usize;
    sorted.get(index).copied()
}

fn level_db(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return -100.0;
    }
    let power = samples
        .iter()
        .map(|&s| f64::from(s) * f64::from(s))
        .sum::<f64>()
        / samples.len() as f64;
    (10.0 * (power / (32768.0 * 32768.0)).log10()).max(-100.0) as f32
}

/// The device thread: opens, swaps and reopens streams as the wanted state changes.
fn devices(
    shared: &Arc<Shared>,
    commands: &Receiver<Control>,
    control: &Sender<Control>,
    mic: &SyncSender<Mic>,
) {
    let host = cpal::default_host();
    let (lost, lost_rx) = mpsc::channel::<()>();
    let mut input: Option<(Option<String>, Stream)> = None;
    let mut output: Option<(Option<String>, Stream)> = None;
    let mut saved = shared.settings();
    let mut retry = false;
    loop {
        let wait = if retry {
            commands.recv_timeout(Duration::from_secs(2))
        } else {
            commands.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        if matches!(
            wait,
            Ok(Control::Stop) | Err(RecvTimeoutError::Disconnected)
        ) {
            return;
        }
        while let Ok(command) = commands.try_recv() {
            if command == Control::Stop {
                return;
            }
        }
        if lost_rx.try_recv().is_ok() {
            while lost_rx.try_recv().is_ok() {}
            input = None;
            output = None;
        }
        let settings = shared.settings();
        if settings != saved {
            saved = settings.clone();
            save(shared);
        }
        let voice = shared.voice.load(Relaxed);
        let testing = shared.testing.load(Relaxed);
        if testing {
            *lock(&shared.devices) = list(&host);
        }
        let mut problem = None;
        let want_input = (voice || testing).then_some(settings.input);
        if want_input.as_ref() != input.as_ref().map(|(id, _)| id) {
            input = None;
            if let Some(id) = want_input {
                match open_input(
                    &host,
                    shared,
                    id.as_deref(),
                    mic.clone(),
                    on_error(lost.clone(), control.clone()),
                ) {
                    Ok(stream) => input = Some((id, stream)),
                    Err(error) => {
                        tracing::warn!(error = %format_args!("{error:#}"), "opening microphone failed");
                        problem = Some("Can't use your microphone. Pick another one in Settings.");
                    }
                }
            }
        }
        let want_output = voice.then_some(settings.output);
        if want_output.as_ref() != output.as_ref().map(|(id, _)| id) {
            output = None;
            if let Some(id) = want_output {
                match open_output(
                    &host,
                    shared,
                    id.as_deref(),
                    mic.clone(),
                    on_error(lost.clone(), control.clone()),
                ) {
                    Ok(stream) => output = Some((id, stream)),
                    Err(error) => {
                        tracing::warn!(error = %format_args!("{error:#}"), "opening speakers failed");
                        problem = Some("Can't use your speakers. Pick others in Settings.");
                    }
                }
            }
        }
        retry = problem.is_some();
        *lock(&shared.problem) = problem.map(str::to_owned);
    }
}

fn save(shared: &Shared) {
    let result = serde_json::to_vec_pretty(&shared.settings())
        .map_err(anyhow::Error::from)
        .and_then(|bytes| Ok(fs::write(&shared.file, bytes)?));
    if let Err(error) = result {
        tracing::error!(error = %format_args!("{error:#}"), "saving voice settings failed");
    }
}

/// Devices to pick from; speaker monitors are hidden from the microphones.
fn list(host: &Host) -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
    let list = |devices: Result<_, cpal::Error>| -> Vec<DeviceInfo> {
        let devices: Vec<Device> = devices.map(Iterator::collect).unwrap_or_else(|error| {
            tracing::warn!(%error, "listing audio devices failed");
            Vec::new()
        });
        devices
            .into_iter()
            .filter_map(|device| {
                let id = device.id().ok()?.to_string();
                let name = device.description().ok()?.name().to_owned();
                (!id.ends_with(".monitor")).then_some(DeviceInfo { id, name })
            })
            .collect()
    };
    (list(host.input_devices()), list(host.output_devices()))
}

fn pick(host: &Host, id: Option<&str>, input: bool) -> Result<Device> {
    let chosen = id.and_then(|id| host.device_by_id(&id.parse::<DeviceId>().ok()?));
    if id.is_some() && chosen.is_none() {
        tracing::warn!("saved audio device missing, using the default");
    }
    chosen
        .or_else(|| {
            if input {
                host.default_input_device()
            } else {
                host.default_output_device()
            }
        })
        .ok_or_else(|| anyhow!("no audio device"))
}

/// 48 kHz in a sample format we convert, when the device offers it; else its default.
fn config(device: &Device, input: bool) -> Result<SupportedStreamConfig> {
    let ranges: Vec<_> = if input {
        device.supported_input_configs()?.collect()
    } else {
        device.supported_output_configs()?.collect()
    };
    let at_rate = [SampleFormat::F32, SampleFormat::I16, SampleFormat::I32]
        .into_iter()
        .find_map(|format| {
            ranges
                .iter()
                .filter(|range| range.sample_format() == format)
                .find_map(|range| range.try_with_sample_rate(RATE))
        });
    match at_rate {
        Some(config) => Ok(config),
        None if input => Ok(device.default_input_config()?),
        None => Ok(device.default_output_config()?),
    }
}

/// 10 ms device buffers where the device lets us pick; some defaults hold seconds.
fn low_latency(supported: &SupportedStreamConfig) -> StreamConfig {
    let buffer_size = match *supported.buffer_size() {
        SupportedBufferSize::Range { min, max } => {
            BufferSize::Fixed((supported.sample_rate() / 100).clamp(min, max))
        }
        SupportedBufferSize::Unknown => BufferSize::Default,
    };
    StreamConfig {
        buffer_size,
        ..supported.config()
    }
}

fn on_error(
    lost: Sender<()>,
    control: Sender<Control>,
) -> impl FnMut(cpal::Error) + Send + 'static {
    move |error| {
        // Glitches and followed default-device switches keep the stream running.
        if matches!(
            error.kind(),
            ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
        ) {
            tracing::debug!(%error, "audio device hiccup");
            return;
        }
        tracing::warn!(%error, "audio device failed");
        let _ = lost.send(());
        let _ = control.send(Control::Refresh);
    }
}

fn open_input(
    host: &Host,
    shared: &Arc<Shared>,
    id: Option<&str>,
    mic: SyncSender<Mic>,
    error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream> {
    let device = pick(host, id, true)?;
    let supported = config(&device, true)?;
    let format = supported.sample_format();
    let config = low_latency(&supported);
    tracing::info!(
        rate = config.sample_rate,
        channels = config.channels,
        ?format,
        "microphone opened"
    );
    let stream = match format {
        SampleFormat::F32 => capture::<f32>(&device, config, shared, mic, error),
        SampleFormat::I16 => capture::<i16>(&device, config, shared, mic, error),
        SampleFormat::I32 => capture::<i32>(&device, config, shared, mic, error),
        other => return Err(anyhow!("unsupported sample format {other}")),
    }?;
    stream.play()?;
    Ok(stream)
}

fn capture<T: SizedSample>(
    device: &Device,
    config: StreamConfig,
    shared: &Arc<Shared>,
    mic: SyncSender<Mic>,
    error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream>
where
    i16: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    let shared = shared.clone();
    Ok(device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            let at = info.timestamp();
            let delay = at.callback.duration_since(at.capture).as_millis() as u32;
            shared.input_delay.store(delay, Relaxed);
            for block in data.chunks(FRAME * channels) {
                let mut samples = [0; FRAME];
                for (slot, frame) in samples.iter_mut().zip(block.chunks(channels)) {
                    let sum: i32 = frame.iter().map(|&s| i32::from(i16::from_sample(s))).sum();
                    *slot = (sum / channels as i32) as i16;
                }
                let len = block.len() / channels;
                let _ = mic.try_send(Mic::Near { samples, len, rate });
            }
        },
        error,
        None,
    )?)
}

fn open_output(
    host: &Host,
    shared: &Arc<Shared>,
    id: Option<&str>,
    mic: SyncSender<Mic>,
    error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream> {
    let device = pick(host, id, false)?;
    let supported = config(&device, false)?;
    let format = supported.sample_format();
    let config = low_latency(&supported);
    tracing::info!(
        rate = config.sample_rate,
        channels = config.channels,
        ?format,
        "speakers opened"
    );
    let stream = match format {
        SampleFormat::F32 => play::<f32>(&device, config, shared, mic, error),
        SampleFormat::I16 => play::<i16>(&device, config, shared, mic, error),
        SampleFormat::I32 => play::<i32>(&device, config, shared, mic, error),
        other => return Err(anyhow!("unsupported sample format {other}")),
    }?;
    stream.play()?;
    Ok(stream)
}

fn play<T: SizedSample + FromSample<i16>>(
    device: &Device,
    config: StreamConfig,
    shared: &Arc<Shared>,
    mic: SyncSender<Mic>,
    error: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream> {
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    let shared = shared.clone();
    let mut resampler = AudioResampler::default();
    let mut ready = VecDeque::new();
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let at = info.timestamp();
            let delay = at.playback.duration_since(at.callback).as_millis() as u32;
            shared.output_delay.store(delay, Relaxed);
            let frames = data.len() / channels;
            while ready.len() < frames {
                let chunk = mix(&shared);
                if rate == RATE {
                    ready.extend(&chunk);
                } else {
                    ready.extend(resampler.remix_and_resample(
                        &chunk,
                        FRAME as u32,
                        1,
                        RATE,
                        1,
                        rate,
                    ));
                }
                let _ = mic.try_send(Mic::Far(chunk));
            }
            for (frame, sample) in data.chunks_mut(channels).zip(ready.drain(..frames)) {
                frame.fill(T::from_sample(sample));
            }
        },
        error,
        None,
    )?)
}

/// 10 ms of all tracks at their streams' volumes.
fn mix(shared: &Shared) -> [i16; FRAME] {
    let mut sum = [0f32; FRAME];
    let deafened = shared.deafened.load(Relaxed);
    let mut mixer = lock(&shared.mixer);
    let Mixer {
        tracks, volumes, ..
    } = &mut *mixer;
    for track in tracks.values_mut() {
        let (volume, muted) = volumes.get(&track.stream).copied().unwrap_or((1.0, false));
        let gain = if deafened || muted { 0.0 } else { volume };
        track.primed |= track.queue.len() >= PRIME;
        if !track.primed {
            continue;
        }
        // The sender's clock and the sound card's drift apart: drop or repeat one sample
        // per 10 ms to keep the queue near PRIME, too small a step to hear.
        if track.queue.len() > PRIME + FRAME {
            track.queue.pop_front();
        } else if track.queue.len() < 2 * FRAME
            && let Some(&last) = track.queue.back()
        {
            // Less than a frame would remain after this one.
            track.queue.push_back(last);
        }
        for slot in &mut sum {
            let Some(sample) = track.queue.pop_front() else {
                track.primed = false;
                break;
            };
            *slot += f32::from(sample) * gain;
        }
    }
    sum.map(|s| s.clamp(-32768.0, 32767.0) as i16)
}

/// The microphone thread: resample, process, gate, and hand frames to the source.
fn process(shared: &Shared, frames: &Receiver<Mic>) {
    let mut built = u64::MAX;
    let mut apm = None;
    // Captured samples at the device rate, then 48 kHz samples waiting for a full frame.
    let mut pending = Vec::new();
    let mut near = Vec::new();
    let mut resampler = AudioResampler::default();
    let mut hold = 0;
    // A stronger AI noise filter can go here later, after the WebRTC processing.
    let mut filter: Option<Filter> = None;
    while let Ok(message) = frames.recv() {
        let wanted = shared.processing.load(Relaxed);
        if wanted != built {
            let s = shared.settings();
            built = wanted;
            apm = Some(AudioProcessingModule::new(
                s.echo_cancellation,
                s.auto_gain,
                true,
                s.noise_suppression,
            ));
        }
        let apm = apm.as_mut().expect("built above");
        match message {
            Mic::Far(mut chunk) => {
                if let Err(error) = apm.process_reverse_stream(&mut chunk, RATE as i32, 1) {
                    tracing::warn!(%error, "echo reference failed");
                }
                continue;
            }
            Mic::Near { samples, len, rate } => {
                let step = rate as usize / 100;
                pending.extend_from_slice(&samples[..len]);
                while pending.len() >= step {
                    let chunk: Vec<_> = pending.drain(..step).collect();
                    if rate == RATE {
                        near.extend(chunk);
                    } else {
                        near.extend(resampler.remix_and_resample(
                            &chunk,
                            step as u32,
                            1,
                            rate,
                            1,
                            RATE,
                        ));
                    }
                }
            }
        }
        while near.len() >= FRAME {
            let mut frame: Vec<i16> = near.drain(..FRAME).collect();
            let delay = shared.input_delay.load(Relaxed) + shared.output_delay.load(Relaxed);
            let _ = apm.set_stream_delay_ms(delay as i32);
            if let Err(error) = apm.process_stream(&mut frame, RATE as i32, 1) {
                tracing::warn!(%error, "audio processing failed");
            }
            if let Some(filter) = &mut filter {
                filter(&mut frame);
            }
            let db = level_db(&frame);
            shared.level.store(db.to_bits(), Relaxed);
            if let Some(levels) = lock(&shared.record).as_mut() {
                levels.push(db);
            }
            let (push_to_talk, threshold) = {
                let s = lock(&shared.settings);
                (s.push_to_talk, s.threshold)
            };
            hold = if db > threshold {
                HOLD_FRAMES
            } else {
                hold.saturating_sub(1)
            };
            let open = !shared.muted.load(Relaxed)
                && !shared.deafened.load(Relaxed)
                && if push_to_talk {
                    shared.key_down.load(Relaxed)
                } else {
                    hold > 0
                };
            if open && db > SILENT_DB {
                shared.talked.store(shared.now(), Relaxed);
            }
            if !open {
                frame.fill(0);
            }
            if shared.voice.load(Relaxed) {
                send(&shared.source, &frame);
            }
        }
    }
}

/// The source copies 10 ms frames synchronously when it has no queue.
fn send(source: &NativeAudioSource, frame: &[i16]) {
    let audio = AudioFrame {
        data: Cow::Borrowed(frame),
        sample_rate: RATE,
        num_channels: 1,
        samples_per_channel: FRAME as u32,
    };
    let future = pin!(source.capture_frame(&audio));
    match future.poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(Ok(())) => {}
        Poll::Ready(Err(error)) => tracing::warn!(%error, "sending microphone audio failed"),
        Poll::Pending => tracing::warn!("microphone source is buffering"),
    }
}
