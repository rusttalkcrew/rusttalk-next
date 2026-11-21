//! Voice audio on its own threads, so device work never blocks the UI.
//!
//! Microphone: cpal capture → 48 kHz mono → WebRTC audio processing (echo
//! cancellation and high-pass) → classic or AI noise removal → pre-gain level and VAD → bounded gain →
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
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed},
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
    denoise::{self, Worker},
    mic_check::{self, Verdict},
    server::Id,
    sounds::{self, Cue},
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
pub const AUTO_MIN_DB: f32 = -45.0;
pub const AUTO_MAX_DB: f32 = -25.0;
/// The noise floor is a low percentile of the last 5 s, in 100 ms blocks.
const NOISE_BLOCK: u32 = 10;
const NOISE_BLOCKS: usize = 50;
const NOISE_PERCENTILE: f32 = 0.1;
/// The floor rises at most this fast, in dB per block,
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
    pub call_sounds: sounds::Preferences,
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
    pub noise_strength: u8,
    /// Suggest fixes during a call when the microphone is noisy or too loud;
    /// "Don't ask again" turns it off. This device only.
    pub mic_hint: bool,
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
    /// Allow DMs from friends only: others land under "Message requests" without
    /// notifications. Enforced here, since servers don't know who is a friend.
    pub friends_only: bool,
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
    /// Hide from member lists while offline; this device's copy of the per-server flag.
    pub hide_offline: crate::server::HideOffline,
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
            call_sounds: sounds::Preferences::default(),
            push_to_talk: false,
            key: "V".into(),
            threshold: -42.0,
            auto_threshold: true,
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain: true,
            ai_noise: false,
            noise_strength: 24,
            mic_hint: true,
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
            friends_only: false,
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
            hide_offline: Default::default(),
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
    /// The microphone problem noticed in this call, until the UI takes it (`trouble`).
    trouble: AtomicU8,
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
    cues: VecDeque<i16>,
    cue_until: Option<Instant>,
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
            trouble: AtomicU8::new(0),
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
        let toggles = |s: &Settings| (s.echo_cancellation, s.noise_suppression, s.ai_noise);
        if toggles(&current) != toggles(&settings) {
            self.shared().processing.fetch_add(1, Relaxed);
        }
        if current.call_sounds != settings.call_sounds {
            let mut mixer = lock(&self.shared().mixer);
            mixer.cues.clear();
            mixer.cue_until = None;
        }
        *current = settings;
        drop(current);
        self.refresh();
    }

    /// Joined a voice room.
    pub fn start_voice(&self) {
        self.shared().voice.store(true, Relaxed);
        self.refresh();
    }

    /// Drop the old room's audio before queuing its leave cue.
    pub fn end_voice(&self, cue: Option<Cue>) {
        {
            let mut mixer = lock(&self.shared().mixer);
            mixer.tracks.clear();
            mixer.cues.clear();
            mixer.cue_until = None;
        }
        if let Some(cue) = cue {
            self.play_cue(cue);
        }
        self.shared().voice.store(false, Relaxed);
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

    /// Plays the matching chime once per change. Not while deafened: deafening already
    /// mutes, and its own chime covers that.
    pub fn set_muted(&self, on: bool) {
        if self.shared().muted.swap(on, Relaxed) != on && !self.deafened() {
            self.play_cue(if on { Cue::Muted } else { Cue::Unmuted });
        }
    }

    pub fn deafened(&self) -> bool {
        self.shared().deafened.load(Relaxed)
    }

    /// Plays the matching chime once per change, even when deafening.
    pub fn set_deafened(&self, on: bool) {
        if self.shared().deafened.swap(on, Relaxed) == on {
            return;
        }
        if on {
            let mut mixer = lock(&self.shared().mixer);
            mixer.cues.clear();
            mixer.cue_until = None;
        }
        self.play_cue(if on { Cue::Deafened } else { Cue::Undeafened });
    }

    /// Queue a chime in the speaker mixer only. Echo cancellation gets the same
    /// reference it gets for remote audio, including any sound from the speakers.
    pub fn play_cue(&self, cue: Cue) {
        let settings = lock(&self.shared().settings);
        let prefs = &settings.call_sounds;
        // Deafened people only hear the chimes about deafening itself.
        let silenced = self.deafened() && !matches!(cue, Cue::Deafened | Cue::Undeafened);
        if !prefs.enabled || prefs.volume <= 0.0 || silenced || prefs.disabled.contains(&cue) {
            return;
        }
        let mut mixer = lock(&self.shared().mixer);
        let now = Instant::now();
        if mixer.cue_until.is_some_and(|until| until <= now) {
            mixer.cues.clear();
        }
        // Keep cues timely if several people join at once or the output stalls.
        if mixer.cues.len() < RATE as usize / 2 {
            mixer.cues.extend(cue.samples(RATE, prefs.volume));
            mixer.cue_until = Some(now + Duration::from_secs(1));
        }
    }

    /// The last microphone level in dBFS.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared().level.load(Relaxed))
    }

    /// The voice activation level automatic sensitivity picked last, in dBFS.
    pub fn auto_threshold(&self) -> f32 {
        f32::from_bits(self.shared().auto_threshold.load(Relaxed))
    }

    /// The microphone problem noticed in this call, once; `None` until there is one.
    pub fn take_trouble(&self) -> Option<Verdict> {
        match self.shared().trouble.swap(0, Relaxed) {
            1 => Some(Verdict::Noisy),
            2 => Some(Verdict::Loud),
            _ => None,
        }
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

    /// Whose a playing track is, once known: a device that joined while it couldn't be
    /// named plays as nobody until then, out of reach of their volume and mute.
    pub fn set_person(&self, sid: &str, person: Id) {
        lock(&self.shared().mixer).set_person(sid, person);
    }
}

impl Mixer {
    fn set_person(&mut self, sid: &str, person: Id) {
        if let Some(track) = self.tracks.get_mut(sid) {
            track.stream.0 = person;
        }
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
        let block = self.sum / NOISE_BLOCK as f32;
        self.sum = 0.0;
        self.frames = 0;
        self.blocks[self.next] = block;
        self.next = (self.next + 1) % NOISE_BLOCKS;
        self.filled = (self.filled + 1).min(NOISE_BLOCKS);
        // Keep quiet gaps in the percentile so speech alone cannot become the noise floor.
        // Hold the learned sensitivity when the mic is too quiet to measure room tone.
        if block < AUTO_MIN_DB - AUTO_MARGIN_DB {
            return;
        }
        // Ten times a second over 50 values: trivial next to the audio processing.
        let target = percentile(&self.blocks[..self.filled], NOISE_PERCENTILE).expect("filled");
        self.floor = target
            .max(AUTO_MIN_DB - AUTO_MARGIN_DB)
            .clamp(self.floor - 0.2, self.floor + NOISE_RISE_DB);
    }

    fn floor(&self) -> f32 {
        self.floor
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
    let mut close_output_at = None::<Instant>;
    loop {
        let wait = match (close_output_at, retry) {
            (Some(until), _) => {
                commands.recv_timeout(until.saturating_duration_since(Instant::now()))
            }
            (None, true) => commands.recv_timeout(Duration::from_secs(2)),
            (None, false) => commands.recv().map_err(|_| RecvTimeoutError::Disconnected),
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
        // A leave cue can keep speakers open after the microphone closes.
        let playing_cue = lock(&shared.mixer)
            .cue_until
            .filter(|until| *until > Instant::now());
        close_output_at = if voice { None } else { playing_cue };
        let want_output = (voice || playing_cue.is_some()).then_some(settings.output);
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

fn pick(host: &Host, id: Option<&str>, input: bool) -> Result<(Device, bool)> {
    let chosen = id.and_then(|id| host.device_by_id(&id.parse::<DeviceId>().ok()?));
    let is_selected = id.is_none() || chosen.is_some();
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
        .map(|device| (device, is_selected))
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
    let (device, _) = pick(host, id, true)?;
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
    let (device, play_cues) = pick(host, id, false)?;
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
        SampleFormat::F32 => play::<f32>(&device, config, shared, mic, error, play_cues),
        SampleFormat::I16 => play::<i16>(&device, config, shared, mic, error, play_cues),
        SampleFormat::I32 => play::<i32>(&device, config, shared, mic, error, play_cues),
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
    play_cues: bool,
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
                let chunk = mix(&shared, play_cues);
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
            // Deafening is applied in the mixer, so its own chime still reaches the speakers.
            for (frame, sample) in data.chunks_mut(channels).zip(ready.drain(..frames)) {
                frame.fill(T::from_sample(sample));
            }
        },
        error,
        None,
    )?)
}

/// 10 ms of all tracks at their streams' volumes.
fn mix(shared: &Shared, play_cues: bool) -> [i16; FRAME] {
    let mut mixer = lock(&shared.mixer);
    // Remote voices can fall back to the default device when a saved device is missing.
    if !play_cues {
        mixer.cues.clear();
    }
    mix_tracks(&mut mixer, shared.deafened.load(Relaxed))
}

fn mix_tracks(mixer: &mut Mixer, deafened: bool) -> [i16; FRAME] {
    let mut sum = [0f32; FRAME];
    let Mixer {
        tracks,
        volumes,
        cues,
        cue_until,
        ..
    } = mixer;
    if cue_until.is_some_and(|until| until <= Instant::now()) {
        cues.clear();
    }
    for (slot, sample) in sum.iter_mut().zip(cues.drain(..cues.len().min(FRAME))) {
        *slot = f32::from(sample);
    }
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

struct VoiceGain(f32);

impl Default for VoiceGain {
    fn default() -> Self {
        Self(1.0)
    }
}

impl VoiceGain {
    fn apply(&mut self, frame: &mut [i16; FRAME], db: f32, threshold: f32) {
        if db <= threshold {
            return;
        }
        // Move toward -18 dBFS slowly for quiet speech, quickly for loud speech.
        let target = 10f32.powf((-18.0 - db) / 20.0).clamp(0.25, 4.0);
        self.0 += (target - self.0) * if target < self.0 { 0.25 } else { 0.005 };
        let peak = frame
            .iter()
            .map(|x| x.unsigned_abs())
            .max()
            .unwrap_or(1)
            .max(1);
        let limited = self.0.min(32767.0 / f32::from(peak));
        for x in frame {
            *x = (f32::from(*x) * limited) as i16;
        }
    }
}

/// The microphone thread: resample, process, gate, and hand frames to the source.
fn process(shared: &Shared, frames: &Receiver<Mic>) {
    let mut built = u64::MAX;
    let mut apm = None;
    let mut denoiser: Option<Worker> = None;
    let mut loading: Option<thread::JoinHandle<Result<denoise::Denoiser>>> = None;
    let mut gain = VoiceGain::default();
    let mut framer = Framer::default();
    let mut gate = Gate::default();
    let mut noise = NoiseFloor::default();
    let mut check = mic_check::Detector::default();
    while let Ok(message) = frames.recv() {
        let wanted = shared.processing.load(Relaxed);
        if wanted != built {
            let s = shared.settings();
            built = wanted;
            if !s.ai_noise {
                denoiser = None;
                loading = None;
                shared.ai_failed.store(false, Relaxed);
            } else if denoiser.is_none()
                && loading.is_none()
                && *lock(&shared.model) == Model::Ready
            {
                let file = shared.model_file.clone();
                loading = Some(thread::spawn(move || denoise::load(&file)));
            }
            apm = Some(build_apm(
                &s,
                denoiser.as_ref().is_some_and(|w| !w.bypassed()),
            ));
        }
        if loading.as_ref().is_some_and(|job| job.is_finished()) {
            match loading
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err(anyhow!("noise worker panicked")))
            {
                Ok(model) => match Worker::start(model) {
                    Ok(worker) => {
                        denoiser = Some(worker);
                        shared.ai_failed.store(false, Relaxed);
                        apm = Some(build_apm(&shared.settings(), true));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "starting AI noise removal worker failed");
                        shared.ai_failed.store(true, Relaxed);
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "loading AI noise removal failed");
                    shared.ai_failed.store(true, Relaxed);
                    if let Err(error) = fs::remove_file(&shared.model_file) {
                        tracing::warn!(%error, "removing unusable noise model failed");
                    }
                    *lock(&shared.model) = Model::Missing;
                }
            }
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
            // Before the gain: clipping happens at the device, and the gain would hide it.
            let clipped = frame
                .iter()
                .any(|s| s.unsigned_abs() >= mic_check::CLIP_LEVEL);
            let delay = shared.input_delay.load(Relaxed) + shared.output_delay.load(Relaxed);
            let _ = apm.set_stream_delay_ms(delay as i32);
            if let Err(error) = apm.process_stream(&mut frame, RATE as i32, 1) {
                tracing::warn!(%error, "audio processing failed");
            }
            // After the echo canceller, which needs the untouched signal and leaves only
            // what the model should judge; the model replaces WebRTC's noise suppression.
            if let Some(worker) = &mut denoiser {
                let active = !worker.bypassed();
                worker.process(&mut frame, lock(&shared.settings).noise_strength);
                if active && worker.bypassed() {
                    shared.ai_failed.store(true, Relaxed);
                    *apm = build_apm(&shared.settings(), false);
                }
            }
            let db = level_db(&frame);
            shared.level.store(db.to_bits(), Relaxed);
            if let Some(levels) = lock(&shared.record).as_mut() {
                levels.push(db);
            }
            noise.push(db);
            let auto = noise.threshold();
            shared.auto_threshold.store(auto.to_bits(), Relaxed);
            let (push_to_talk, threshold, watch, auto_gain) = {
                let s = lock(&shared.settings);
                (
                    s.push_to_talk,
                    if s.auto_threshold { auto } else { s.threshold },
                    s.mic_hint && !s.ai_noise,
                    s.auto_gain,
                )
            };
            if !shared.voice.load(Relaxed) {
                // Out of a call: the next one is judged afresh.
                check = mic_check::Detector::default();
            } else if watch {
                let muted = shared.muted.load(Relaxed) || shared.deafened.load(Relaxed);
                let frame = mic_check::Frame {
                    floor: noise.floor().min(db),
                    clipped,
                    muted,
                };
                if let Some(verdict) = check.push(frame) {
                    let code = if verdict == Verdict::Noisy { 1 } else { 2 };
                    shared.trouble.store(code, Relaxed);
                }
            }
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
            if auto_gain {
                gain.apply(&mut frame, db, threshold);
            } else {
                gain = VoiceGain::default();
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
        false,
        true,
        s.noise_suppression && !ai_noise,
    )
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
            let chunk = &self.pending[..step];
            if rate == RATE {
                self.near.extend(chunk);
            } else {
                self.near.extend(self.resampler.remix_and_resample(
                    chunk,
                    step as u32,
                    1,
                    rate,
                    1,
                    RATE,
                ));
            }
            self.pending.drain(..step);
        }
    }

    fn next(&mut self) -> Option<[i16; FRAME]> {
        (self.near.len() >= FRAME).then(|| std::array::from_fn(|_| self.near.pop_front().unwrap()))
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

    #[test]
    fn a_track_gets_its_person_later() {
        let mut mixer = Mixer::default();
        let track = |stream| Track {
            serial: 1,
            stream,
            queue: VecDeque::new(),
            primed: false,
            loud: 0,
        };
        let app: TrackSid = "TR_app".to_owned().try_into().unwrap();
        mixer.tracks.insert("voice".into(), track(([0; 32], None)));
        mixer
            .tracks
            .insert("app".into(), track(([0; 32], Some(app.clone()))));
        mixer.set_person("voice", [7; 32]);
        mixer.set_person("app", [7; 32]);
        mixer.set_person("gone", [7; 32]);
        assert_eq!(mixer.tracks["voice"].stream, ([7; 32], None));
        assert_eq!(mixer.tracks["app"].stream, ([7; 32], Some(app)));
    }

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
        // And it turns off again: the floor returns gradually.
        feed(&mut noise, &mut gate, -55.0, 15.0);
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
    fn fan_after_two_minutes_of_silence_stays_out() {
        let mut apm = build_apm(&Settings::default(), false);
        let (mut noise, mut gate) = (NoiseFloor::default(), Gate::default());
        let mut gain = VoiceGain::default();
        let mut initial = 0.0;
        let mut speech = 0;
        for i in 0..13200 {
            let mut frame = std::array::from_fn(|j| {
                let phase = (i * FRAME + j) as f32 / RATE as f32 * std::f32::consts::TAU;
                let fan = if (1000..13000).contains(&i) {
                    0.0
                } else {
                    150.0
                };
                (fan * (phase * 100.0).sin()
                    + if (700..780).contains(&i) {
                        2400.0 * (phase * 220.0).sin()
                    } else {
                        0.0
                    }) as i16
            });
            apm.process_stream(&mut frame, RATE as i32, 1).unwrap();
            let db = level_db(&frame);
            noise.push(db);
            let voiced = gate.update(db, noise.threshold());
            let previous = gain.0;
            gain.apply(&mut frame, db, noise.threshold());
            assert!(gain.0 <= 4.0);
            if (700..780).contains(&i) && voiced {
                speech += 1;
            }
            if i == 999 {
                initial = noise.threshold();
            }
            if i == 12999 {
                assert_eq!(initial, noise.threshold());
            }
            if i >= 13000 {
                assert!(!voiced);
                assert_eq!(previous, gain.0);
            }
        }
        assert!(speech >= 75);
        eprintln!(
            "threshold {initial:.1} dBFS before and after silence, 0/200 returning fan frames opened, {speech}/80 speech frames opened"
        );
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
    fn mixer_consumes_cues_and_still_plays_them_while_deafened() {
        let samples: VecDeque<_> = Cue::YouJoined.samples(RATE, 0.5).collect();
        let mut mixer = Mixer {
            cues: samples.clone(),
            ..Mixer::default()
        };
        assert!(
            mix_tracks(&mut mixer, false)
                .iter()
                .any(|&sample| sample != 0)
        );
        assert_eq!(mixer.cues.len(), samples.len() - FRAME);
        // Deafening silences voices, not the chime that confirms it.
        assert!(
            mix_tracks(&mut mixer, true)
                .iter()
                .any(|&sample| sample != 0)
        );
        assert_eq!(mixer.cues.len(), samples.len() - 2 * FRAME);
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
