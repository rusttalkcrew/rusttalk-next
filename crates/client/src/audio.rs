//! Voice audio on its own threads, so device work never blocks the UI.
//!
//! Microphone: cpal capture → 48 kHz mono → WebRTC audio processing (echo
//! cancellation, noise suppression, auto gain) → optional AI noise removal →
//! push-to-talk or voice gate → the `NativeAudioSource` that voice publishes.
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
use rusttalk_core::client::save_json;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use crate::{
    denoise::{self, Denoiser},
    server::Id,
};

pub const RATE: u32 = 48_000;
/// 10 ms at 48 kHz mono, the frame size of audio processing, AI noise removal and the source.
pub const FRAME: usize = 480;
/// Quieter than this counts as silence for the meter and speaking rings.
pub const SILENT_DB: f32 = -50.0;
/// Voice activation stays open this long after the last loud frame.
const HOLD_FRAMES: u32 = 40;
/// Once open, the gate closes only this far below its threshold, so it doesn't chatter.
const HYSTERESIS_DB: f32 = 4.0;
/// Automatic sensitivity opens this far above the background noise.
const AUTO_MARGIN_DB: f32 = 10.0;
/// The automatic threshold stays inside this range, in dBFS.
pub const AUTO_MIN_DB: f32 = -60.0;
pub const AUTO_MAX_DB: f32 = -25.0;
/// The noise floor is a low percentile of the last 5 s, in 100 ms blocks.
const NOISE_BLOCK: u32 = 10;
const NOISE_BLOCKS: usize = 50;
const NOISE_PERCENTILE: f32 = 0.1;
/// The floor follows quieter noise at once but rises at most this fast, in dB per block,
/// so speech can't drag it up while a lasting louder noise still catches up in seconds.
const NOISE_RISE_DB: f32 = 1.0;
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
    /// Voice activation level in dBFS, used while `auto_threshold` is off.
    pub threshold: f32,
    /// Adjust the voice activation level to the background noise. On for new
    /// settings; a saved file without it keeps its manual level.
    #[serde(default)]
    pub auto_threshold: bool,
    pub echo_cancellation: bool,
    pub noise_suppression: bool,
    pub auto_gain: bool,
    /// AI noise removal in place of the normal noise suppression, once its model is
    /// downloaded. Not synced: the download is per device.
    pub ai_noise: bool,
    /// Camera name; `None` uses the first camera.
    pub camera: Option<String>,
    pub camera_height: u32,
    pub camera_fps: u32,
    /// The server directory for "Find servers"; empty until the person sets one.
    pub index_url: String,
    /// Its certificate pin (base64url), for a directory without a public certificate.
    pub index_fingerprint: String,
    /// Server IDs of official servers the person dismissed from the join page.
    pub dismissed_official: Vec<String>,
    /// "dark", "light" or "system".
    pub theme_mode: String,
    /// The dark and light theme and the accent colour, by ID (see `ui::theme`).
    pub dark_theme: String,
    pub light_theme: String,
    pub accent: String,
    /// A language ID (see `i18n`), or "system" for the computer's language.
    pub language: String,
    /// Sidebar widths and the member list toggle; this device only, never synced.
    pub channels_width: f32,
    pub members_width: f32,
    pub members_hidden: bool,
    /// The voice channel view: chat beside the call instead of under it, the call's
    /// share of the space, which side is hidden and "Show only video"; this device only.
    pub call_side_by_side: bool,
    pub call_split: f32,
    pub call_chat_hidden: bool,
    pub call_stage_hidden: bool,
    pub call_video_only: bool,
    /// The last "Share your screen" choices, kept on this device only: the height
    /// cap (`None` shares at the screen's own size), frame rate and cursor.
    pub screen_height: Option<u32>,
    pub screen_fps: u32,
    pub screen_cursor: bool,
}

impl Settings {
    /// The look these settings pick; unknown IDs fall back to the defaults.
    pub fn appearance(&self) -> crate::ui::Appearance {
        crate::ui::Appearance::from_ids(
            &self.theme_mode,
            &self.dark_theme,
            &self.light_theme,
            &self.accent,
        )
    }

    pub fn set_appearance(&mut self, look: crate::ui::Appearance) {
        self.theme_mode = look.mode.id().into();
        self.dark_theme = look.dark.id.into();
        self.light_theme = look.light.id.into();
        self.accent = look.accent.id.into();
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            push_to_talk: false,
            key: "V".into(),
            threshold: -42.0,
            auto_threshold: true,
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain: true,
            ai_noise: false,
            camera: None,
            camera_height: 720,
            camera_fps: 30,
            index_url: String::new(),
            index_fingerprint: String::new(),
            dismissed_official: Vec::new(),
            theme_mode: "system".into(),
            dark_theme: "midnight".into(),
            light_theme: "daylight".into(),
            accent: "rust".into(),
            language: "system".into(),
            channels_width: crate::home::layout::CHANNELS_WIDTH,
            members_width: crate::home::layout::MEMBERS_WIDTH,
            members_hidden: false,
            call_side_by_side: false,
            call_split: crate::home::layout::SPLIT,
            call_chat_hidden: false,
            call_stage_hidden: false,
            call_video_only: false,
            screen_height: Some(1080),
            screen_fps: 30,
            screen_cursor: true,
        }
    }
}

/// The AI noise removal model on this device.
#[derive(Clone, Copy, PartialEq)]
pub enum Model {
    Missing,
    /// Bytes received so far.
    Downloading(u64),
    /// The last download failed.
    Failed,
    /// Downloaded; it's verified again each time it loads.
    Ready,
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
    /// The automatic voice activation level, in dBFS (f32 bits).
    auto_threshold: AtomicU32,
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
    model_file: PathBuf,
    model: Mutex<Model>,
    /// AI noise removal is wanted but couldn't run, so the normal pipeline does.
    ai_failed: AtomicBool,
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
            // The model download needs the network driver.
            .enable_all()
            .build()
            .context("start playback runtime")?;
        let file = data.join("voice.json");
        let model_file = denoise::path(data);
        let settings = crate::load_json(&file).unwrap_or_else(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "voice settings unreadable, using defaults");
            Settings::default()
        });
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
            auto_threshold: AtomicU32::new(NoiseFloor::default().threshold().to_bits()),
            talked: AtomicU64::new(0),
            input_delay: AtomicU32::new(10),
            output_delay: AtomicU32::new(20),
            record: Mutex::new(None),
            mixer: Mutex::default(),
            devices: Mutex::default(),
            problem: Mutex::new(None),
            model: Mutex::new(if model_file.exists() {
                Model::Ready
            } else {
                Model::Missing
            }),
            model_file,
            ai_failed: AtomicBool::new(false),
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
        let toggles = |s: &Settings| {
            (
                s.echo_cancellation,
                s.noise_suppression,
                s.auto_gain,
                s.ai_noise,
            )
        };
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

    pub fn model(&self) -> Model {
        *lock(&self.shared().model)
    }

    /// AI noise removal is on but couldn't start or stopped working.
    pub fn ai_failed(&self) -> bool {
        self.shared().ai_failed.load(Relaxed)
    }

    /// Fetch the AI noise removal model in the background; the microphone picks it up
    /// once it's verified.
    pub fn download_model(&self) {
        {
            let mut model = lock(&self.shared().model);
            if matches!(*model, Model::Downloading(_) | Model::Ready) {
                return;
            }
            *model = Model::Downloading(0);
        }
        let shared = self.0.shared.clone();
        let Some(runtime) = &self.0.runtime else {
            return;
        };
        runtime.spawn(async move {
            let result = denoise::download(&shared.model_file, |bytes| {
                *lock(&shared.model) = Model::Downloading(bytes);
            })
            .await;
            *lock(&shared.model) = match result {
                Ok(()) => Model::Ready,
                Err(error) => {
                    tracing::warn!(error = %format_args!("{error:#}"), "downloading the noise model failed");
                    Model::Failed
                }
            };
            shared.processing.fetch_add(1, Relaxed);
        });
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

    /// The voice activation level automatic sensitivity picked last, in dBFS.
    pub fn auto_threshold(&self) -> f32 {
        f32::from_bits(self.shared().auto_threshold.load(Relaxed))
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

/// A slow estimate of the background noise from 10 ms frame levels.
struct NoiseFloor {
    /// Ring of 100 ms block averages.
    blocks: [f32; NOISE_BLOCKS],
    filled: usize,
    next: usize,
    sum: f32,
    frames: u32,
    floor: f32,
}

impl Default for NoiseFloor {
    fn default() -> Self {
        Self {
            blocks: [0.0; NOISE_BLOCKS],
            filled: 0,
            next: 0,
            sum: 0.0,
            frames: 0,
            // A quiet room after processing, so the first words after start aren't cut off.
            floor: AUTO_MIN_DB - AUTO_MARGIN_DB,
        }
    }
}

impl NoiseFloor {
    fn push(&mut self, db: f32) {
        self.sum += db;
        self.frames += 1;
        if self.frames < NOISE_BLOCK {
            return;
        }
        self.blocks[self.next] = self.sum / NOISE_BLOCK as f32;
        self.next = (self.next + 1) % NOISE_BLOCKS;
        self.filled = (self.filled + 1).min(NOISE_BLOCKS);
        self.sum = 0.0;
        self.frames = 0;
        // Ten times a second over 50 values: trivial next to the audio processing.
        let target = percentile(&self.blocks[..self.filled], NOISE_PERCENTILE).expect("filled");
        self.floor = target.min(self.floor + NOISE_RISE_DB);
    }

    fn threshold(&self) -> f32 {
        (self.floor + AUTO_MARGIN_DB).clamp(AUTO_MIN_DB, AUTO_MAX_DB)
    }
}

/// Voice activation: opens above the threshold, closes once the level stayed below
/// it minus the hysteresis for the hold time, so word endings aren't clipped.
#[derive(Default)]
struct Gate {
    hold: u32,
}

impl Gate {
    fn update(&mut self, db: f32, threshold: f32) -> bool {
        let open = self.hold > 0;
        self.hold = if db > threshold || (open && db > threshold - HYSTERESIS_DB) {
            HOLD_FRAMES
        } else {
            self.hold.saturating_sub(1)
        };
        self.hold > 0
    }
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
                        problem = Some(t!("audio.mic_failed"));
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
                        problem = Some(t!("audio.speakers_failed"));
                    }
                }
            }
        }
        retry = problem.is_some();
        *lock(&shared.problem) = problem.map(str::to_owned);
    }
}

fn save(shared: &Shared) {
    let dir = shared.file.parent().unwrap_or(Path::new("."));
    if let Err(error) = save_json(dir, &shared.file, &shared.settings()) {
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
    mix_tracks(&mut lock(&shared.mixer), shared.deafened.load(Relaxed))
}

fn mix_tracks(mixer: &mut Mixer, deafened: bool) -> [i16; FRAME] {
    let mut sum = [0f32; FRAME];
    let Mixer {
        tracks, volumes, ..
    } = mixer;
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
    let mut denoiser = None;
    let mut framer = Framer::default();
    let mut gate = Gate::default();
    let mut noise = NoiseFloor::default();
    while let Ok(message) = frames.recv() {
        let wanted = shared.processing.load(Relaxed);
        if wanted != built {
            let s = shared.settings();
            built = wanted;
            denoiser = if s.ai_noise {
                denoiser.take().or_else(|| load_denoiser(shared))
            } else {
                shared.ai_failed.store(false, Relaxed);
                None
            };
            apm = Some(build_apm(&s, denoiser.is_some()));
        }
        let apm = apm.as_mut().expect("built above");
        match message {
            Mic::Far(mut chunk) => {
                if let Err(error) = apm.process_reverse_stream(&mut chunk, RATE as i32, 1) {
                    tracing::warn!(%error, "echo reference failed");
                }
                continue;
            }
            Mic::Near { samples, len, rate } => framer.push(&samples[..len], rate),
        }
        while let Some(mut frame) = framer.next() {
            let delay = shared.input_delay.load(Relaxed) + shared.output_delay.load(Relaxed);
            let _ = apm.set_stream_delay_ms(delay as i32);
            if let Err(error) = apm.process_stream(&mut frame, RATE as i32, 1) {
                tracing::warn!(%error, "audio processing failed");
            }
            // After the echo canceller, which needs the untouched signal and leaves only
            // what the model should judge; the model replaces WebRTC's noise suppression.
            if let Some(model) = &mut denoiser
                && let Err(error) = model.process(&mut frame)
            {
                tracing::warn!(error = %format_args!("{error:#}"), "AI noise removal failed");
                shared.ai_failed.store(true, Relaxed);
                denoiser = None;
                // The normal noise suppression takes over from the next frame.
                *apm = build_apm(&shared.settings(), false);
            }
            let db = level_db(&frame);
            shared.level.store(db.to_bits(), Relaxed);
            if let Some(levels) = lock(&shared.record).as_mut() {
                levels.push(db);
            }
            noise.push(db);
            let auto = noise.threshold();
            shared.auto_threshold.store(auto.to_bits(), Relaxed);
            let (push_to_talk, threshold) = {
                let s = lock(&shared.settings);
                (
                    s.push_to_talk,
                    if s.auto_threshold { auto } else { s.threshold },
                )
            };
            let voiced = gate.update(db, threshold);
            let open = !shared.muted.load(Relaxed)
                && !shared.deafened.load(Relaxed)
                && if push_to_talk {
                    shared.key_down.load(Relaxed)
                } else {
                    voiced
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

fn build_apm(s: &Settings, ai_noise: bool) -> AudioProcessingModule {
    // Two noise filters in a row only add artifacts; the model is the stronger one.
    AudioProcessingModule::new(
        s.echo_cancellation,
        s.auto_gain,
        true,
        s.noise_suppression && !ai_noise,
    )
}

/// The downloaded model, or `None` with the failure noted for the settings page.
fn load_denoiser(shared: &Shared) -> Option<Denoiser> {
    if *lock(&shared.model) != Model::Ready {
        // Waiting for the download.
        return None;
    }
    let started = Instant::now();
    match denoise::load(&shared.model_file) {
        Ok(model) => {
            tracing::info!(ms = started.elapsed().as_millis(), "AI noise removal on");
            shared.ai_failed.store(false, Relaxed);
            Some(model)
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "loading AI noise removal failed");
            shared.ai_failed.store(true, Relaxed);
            // Damaged or replaced on disk: offer the download again.
            if let Err(error) = fs::remove_file(&shared.model_file) {
                tracing::warn!(%error, "removing the noise model failed");
            }
            *lock(&shared.model) = Model::Missing;
            None
        }
    }
}

/// Cuts captured audio, in whatever block sizes the device delivers, into 10 ms
/// frames at 48 kHz for processing and the model.
#[derive(Default)]
struct Framer {
    /// Samples at the device rate, waiting for 10 ms to resample.
    pending: Vec<i16>,
    /// 48 kHz samples waiting for a full frame.
    near: VecDeque<i16>,
    resampler: AudioResampler,
}

impl Framer {
    fn push(&mut self, samples: &[i16], rate: u32) {
        let step = rate as usize / 100;
        self.pending.extend_from_slice(samples);
        while self.pending.len() >= step {
            let chunk: Vec<_> = self.pending.drain(..step).collect();
            if rate == RATE {
                self.near.extend(chunk);
            } else {
                self.near.extend(self.resampler.remix_and_resample(
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

    fn next(&mut self) -> Option<Vec<i16>> {
        (self.near.len() >= FRAME).then(|| self.near.drain(..FRAME).collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `seconds` of 10 ms frames at `db` and returns how many opened the gate.
    fn feed(noise: &mut NoiseFloor, gate: &mut Gate, db: f32, seconds: f32) -> usize {
        (0..(seconds * 100.0) as usize)
            .filter(|_| {
                noise.push(db);
                gate.update(db, noise.threshold())
            })
            .count()
    }

    #[test]
    fn steady_noise_sets_the_threshold_above_it() {
        let (mut noise, mut gate) = (NoiseFloor::default(), Gate::default());
        feed(&mut noise, &mut gate, -50.0, 10.0);
        assert!((noise.threshold() - (-50.0 + AUTO_MARGIN_DB)).abs() < 0.01);
        assert_eq!(feed(&mut noise, &mut gate, -50.0, 5.0), 0);
    }

    #[test]
    fn louder_noise_is_shut_out_within_seconds() {
        let (mut noise, mut gate) = (NoiseFloor::default(), Gate::default());
        feed(&mut noise, &mut gate, -55.0, 10.0);
        // A fan turns on: it may open the gate briefly, then the floor catches up.
        feed(&mut noise, &mut gate, -40.0, 8.0);
        assert!((noise.threshold() - (-40.0 + AUTO_MARGIN_DB)).abs() < 0.01);
        assert_eq!(feed(&mut noise, &mut gate, -40.0, 5.0), 0);
        // And it turns off again: the floor drops back at once.
        feed(&mut noise, &mut gate, -55.0, 5.0);
        assert!((noise.threshold() - (-55.0 + AUTO_MARGIN_DB)).abs() < 0.01);
    }

    #[test]
    fn speech_passes_without_raising_the_floor() {
        let (mut noise, mut gate) = (NoiseFloor::default(), Gate::default());
        feed(&mut noise, &mut gate, -55.0, 5.0);
        for _ in 0..10 {
            // Every frame of a word and the short pause after it go through.
            assert_eq!(feed(&mut noise, &mut gate, -25.0, 1.5), 150);
            assert_eq!(feed(&mut noise, &mut gate, -55.0, 0.3), 30);
            feed(&mut noise, &mut gate, -55.0, 0.5);
        }
        assert!(noise.threshold() < -55.0 + AUTO_MARGIN_DB + 1.0);
        // The gate closes in the silence after speaking.
        assert!(!gate.update(-55.0, noise.threshold()));
    }

    #[test]
    fn speech_after_long_silence_opens_at_once() {
        let (mut noise, mut gate) = (NoiseFloor::default(), Gate::default());
        feed(&mut noise, &mut gate, -100.0, 60.0);
        assert_eq!(noise.threshold(), AUTO_MIN_DB);
        noise.push(-35.0);
        assert!(gate.update(-35.0, noise.threshold()));
    }

    #[test]
    fn gate_holds_and_does_not_chatter() {
        let mut gate = Gate::default();
        // Just under the line doesn't open a closed gate.
        assert!(!gate.update(-42.0, -40.0));
        assert!(gate.update(-30.0, -40.0));
        // Hovering just under the line keeps it open past the hold time.
        for _ in 0..2 * HOLD_FRAMES {
            assert!(gate.update(-42.0, -40.0));
        }
        // Well under it, the gate stays open for the hold time, then closes.
        for _ in 0..HOLD_FRAMES - 1 {
            assert!(gate.update(-60.0, -40.0));
        }
        assert!(!gate.update(-60.0, -40.0));
    }

    #[test]
    fn saved_settings_keep_their_manual_level() {
        assert!(Settings::default().auto_threshold);
        let saved: Settings = serde_json::from_str(r#"{"threshold": -30.0}"#).unwrap();
        assert!(!saved.auto_threshold);
        assert_eq!(saved.threshold, -30.0);
    }

    #[test]
    fn framer_cuts_any_callback_size_into_frames() {
        // 48 kHz devices: the samples come out in order, in whole frames.
        let mut framer = Framer::default();
        let input: Vec<i16> = (0..20 * FRAME as i16).collect();
        let mut output = Vec::new();
        let mut rest = &input[..];
        for size in [128, 441, 1, 480, 1024, 7].into_iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (block, tail) = rest.split_at(size.min(rest.len()));
            rest = tail;
            framer.push(block, RATE);
            while let Some(frame) = framer.next() {
                assert_eq!(frame.len(), FRAME);
                output.extend(frame);
            }
        }
        assert_eq!(output, input);

        // Other rates are resampled 10 ms at a time; nothing waits longer than that.
        let mut framer = Framer::default();
        let mut frames = 0;
        for _ in 0..100 {
            framer.push(&[1000; 441], 44_100);
            while let Some(frame) = framer.next() {
                assert_eq!(frame.len(), FRAME);
                frames += 1;
            }
        }
        assert_eq!(frames, 100);
    }

    #[test]
    fn mix_applies_each_stream_volume() {
        let loud: StreamId = ([1; 32], None);
        let quiet: StreamId = ([2; 32], None);
        let mut mixer = Mixer::default();
        mixer.volumes.insert(loud.clone(), (1.0, false));
        mixer.volumes.insert(quiet.clone(), (0.25, false));
        let track = |stream, sample| Track {
            serial: 0,
            stream,
            queue: VecDeque::from(vec![sample; PRIME]),
            primed: false,
            loud: 0,
        };
        mixer.tracks.insert("a".into(), track(loud.clone(), 8000));
        let alone = mix_tracks(&mut mixer, false);
        assert!(alone.iter().all(|&s| s == 8000));

        mixer.tracks.clear();
        mixer.tracks.insert("b".into(), track(quiet, 8000));
        let quarter = mix_tracks(&mut mixer, false);
        assert!(quarter.iter().all(|&s| s == 2000));

        assert!(mix_tracks(&mut mixer, true).iter().all(|&s| s == 0));
    }
}
