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
use livekit::webrtc::{
    audio_frame::AudioFrame,
    audio_source::{AudioSourceOptions, native::NativeAudioSource},
    audio_stream::native::NativeAudioStream,
    native::{apm::AudioProcessingModule, audio_resampler::AudioResampler},
    prelude::RtcAudioTrack,
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
const AUTO_MARGIN_DB: f32 = 14.0;
/// The automatic threshold stays inside this range, in dBFS.
pub const AUTO_MIN_DB: f32 = -55.0;
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
    /// The overlays on screen tiles, for all of them; this device only.
    pub call_show_fps: bool,
    pub call_nerd_stats: bool,
    /// The last "Share your screen" choices, kept on this device only: the height
    /// cap (`None` shares at the screen's own size), frame rate and cursor.
    pub screen_height: Option<u32>,
    pub screen_fps: u32,
    pub screen_cursor: bool,
    /// Hide from member lists while offline; this device's copy of the per-server flag.
    pub hide_offline: crate::server::HideOffline,
    /// Server emoji and stickers in chat play only under the pointer, not all the time.
    /// This device only, never synced.
    pub emoji_on_hover: bool,
    /// Link previews by site (see `previews`): missing until the person answers the
    /// first card's question. This device only, never synced.
    pub link_previews: std::collections::BTreeMap<String, bool>,
    /// Make previews of other links when sending them (see `link_cards`): `None` asks
    /// each time until the person picks Always (`true`) or Never (`false`).
    pub create_link_previews: Option<bool>,
    /// Global shortcuts (see `hotkeys`). This device only: keyboards and desktops differ.
    pub hotkeys: crate::hotkeys::Bindings,
    /// Volumes and mutes per person and shared app, and the masters; this device only.
    pub mix: crate::call::mixer::Mix,
    /// The warning before sharing a terminal was accepted once. This device only.
    pub terminal_warned: bool,
    /// This Android device may host a terminal in a call. Never synced.
    pub android_terminal_hosting: bool,
    /// The app version whose "What's new" was shown last, so it opens once per update.
    /// This device only.
    pub whats_new_seen: String,
    /// The name used on every server without a name of its own, and the profile bio
    /// (see `profile`). Synced, like the overrides.
    pub display_name: String,
    pub bio: String,
    /// Addresses of the servers where this person picked a name of their own.
    pub name_overrides: Vec<String>,
    /// What servers still have to be told about name and bio changes made here.
    pub profile_pending: crate::profile::Pending,
    /// The SIP account for phone calls into a voice room; its password is in the
    /// keychain (see `phone`). This device only, never synced.
    pub phone: crate::phone::Account,
    /// People to call by phone. This device only, never synced.
    pub phone_contacts: Vec<crate::phone::Contact>,
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
            call_show_fps: false,
            call_nerd_stats: false,
            screen_height: Some(1080),
            screen_fps: 30,
            screen_cursor: true,
            hide_offline: Default::default(),
            emoji_on_hover: false,
            link_previews: Default::default(),
            create_link_previews: None,
            hotkeys: Default::default(),
            mix: Default::default(),
            terminal_warned: false,
            android_terminal_hosting: false,
            whats_new_seen: String::new(),
            display_name: String::new(),
            bio: String::new(),
            name_overrides: Vec::new(),
            profile_pending: Default::default(),
            phone: Default::default(),
            phone_contacts: Vec::new(),
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
    file: Mutex<Option<PathBuf>>,
    settings: Mutex<Settings>,
    /// Bumped when the processing toggles change, so the mic thread rebuilds its module.
    processing: AtomicU64,
    /// Each call or AI switch gets a fresh recovery budget, even while capture is idle.
    ai_session: AtomicU64,
    /// In a voice room: open both devices and feed the source.
    voice: AtomicBool,
    #[cfg(target_os = "ios")]
    ios_session: Mutex<Option<Arc<crate::ios::voice::Session>>>,
    #[cfg(target_os = "ios")]
    capture_frames: AtomicU32,
    #[cfg(target_os = "ios")]
    last_capture_ms: AtomicU64,
    /// Settings are open: open the microphone for the meter.
    testing: AtomicBool,
    /// Push-to-talk key held while the window has focus.
    key_down: AtomicBool,
    /// The global push-to-talk shortcut is held, set straight from its listener so it
    /// doesn't wait for a frame while the window is hidden.
    hotkey_down: AtomicBool,
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
    /// What a phone call bridged into the room hears (see `PhoneLine`).
    phone: Mutex<Option<PhoneTap>>,
    phone_withdrawal: AtomicBool,
    phone_withdrawal_requests: std::sync::atomic::AtomicUsize,
}

/// The mixer key of a bridged phone call's sound, kept out of what the phone hears.
const PHONE_TRACK: &str = "phone";
/// The most the phone's share of the call may fall behind, in samples.
const PHONE_LAG: usize = RATE as usize / 5;

/// The room's voices and this microphone, at 48 kHz, waiting for the phone.
#[derive(Default)]
struct PhoneTap {
    allowed: std::collections::HashSet<String>,
    room: VecDeque<i16>,
    mic: VecDeque<i16>,
}

impl PhoneTap {
    fn push(queue: &mut VecDeque<i16>, samples: impl IntoIterator<Item = i16>) {
        queue.extend(samples);
        if queue.len() > PHONE_LAG {
            let extra = queue.len() - PHONE_LAG;
            queue.drain(..extra);
        }
    }
}

/// A phone call bridged into this call: what the phone says plays here, and the room's
/// voices and this microphone go to the phone. Dropping it ends both.
pub struct PhoneLine {
    shared: Arc<Shared>,
    serial: u64,
}

impl PhoneLine {
    /// Play what the phone says, 48 kHz mono.
    pub fn play(&self, samples: &[i16]) {
        let mut mixer = lock(&self.shared.mixer);
        let Some(track) = (mixer.tracks.get_mut(PHONE_TRACK)).filter(|t| t.serial == self.serial)
        else {
            return;
        };
        track.queue.extend(samples);
        if track.queue.len() > CAP {
            let extra = track.queue.len() - PRIME;
            track.queue.drain(..extra);
        }
        if level_db(samples) > SILENT_DB {
            track.loud = self.shared.now();
        }
    }

    /// Fill `out` with what the phone hears next; silence where nothing is queued.
    pub fn take(&self, out: &mut [i16]) {
        let mut tap = lock(&self.shared.phone);
        let Some(tap) = tap.as_mut() else {
            out.fill(0);
            return;
        };
        for slot in out {
            let room = tap.room.pop_front().unwrap_or(0);
            let mic = tap.mic.pop_front().unwrap_or(0);
            *slot = room.saturating_add(mic);
        }
    }
}

impl Drop for PhoneLine {
    fn drop(&mut self) {
        let mut mixer = lock(&self.shared.mixer);
        if mixer
            .tracks
            .get(PHONE_TRACK)
            .is_some_and(|t| t.serial == self.serial)
        {
            mixer.tracks.remove(PHONE_TRACK);
            *lock(&self.shared.phone) = None;
        }
    }
}

/// A person's voice (`None`) or one app they share, by its name.
pub type StreamId = (Id, Option<String>);

#[derive(Default)]
struct Mixer {
    listening: std::collections::HashSet<String>,
    tracks: HashMap<String, Track>,
    serial: u64,
    cues: VecDeque<i16>,
    cue_until: Option<Instant>,
}

struct Track {
    serial: u64,
    stream: StreamId,
    /// Its stream's volume from the mix; 0 when muted.
    gain: f32,
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
            file: Mutex::new(Some(file)),
            settings: Mutex::new(settings),
            processing: AtomicU64::new(0),
            ai_session: AtomicU64::new(0),
            voice: AtomicBool::new(false),
            #[cfg(target_os = "ios")]
            ios_session: Mutex::default(),
            #[cfg(target_os = "ios")]
            capture_frames: AtomicU32::new(0),
            #[cfg(target_os = "ios")]
            last_capture_ms: AtomicU64::new(0),
            testing: AtomicBool::new(false),
            key_down: AtomicBool::new(false),
            hotkey_down: AtomicBool::new(false),
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
            phone: Mutex::default(),
            phone_withdrawal: AtomicBool::new(false),
            phone_withdrawal_requests: std::sync::atomic::AtomicUsize::new(0),
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

    pub fn can_host_terminal(&self) -> bool {
        cfg!(any(target_os = "linux", target_os = "macos", windows))
            || (cfg!(target_os = "android")
                && lock(&self.shared().settings).android_terminal_hosting)
    }

    pub fn settings(&self) -> Settings {
        self.shared().settings()
    }

    /// Wait for any settings write and prevent the old device thread from saving again.
    pub fn stop_saving(&self) {
        lock(&self.shared().file).take();
    }

    pub fn set_settings(&self, settings: Settings) {
        let mut current = lock(&self.shared().settings);
        if *current == settings {
            return;
        }
        let toggles = |s: &Settings| (s.echo_cancellation, s.noise_suppression, s.ai_noise);
        if current.ai_noise != settings.ai_noise {
            self.shared().ai_session.fetch_add(1, Relaxed);
        }
        if toggles(&current) != toggles(&settings) {
            self.shared().processing.fetch_add(1, Relaxed);
        }
        if current.call_sounds != settings.call_sounds {
            let mut mixer = lock(&self.shared().mixer);
            mixer.cues.clear();
            mixer.cue_until = None;
        }
        if current.mix != settings.mix {
            for track in lock(&self.shared().mixer).tracks.values_mut() {
                track.gain = settings.mix.gain(&track.stream);
            }
        }
        *current = settings;
        drop(current);
        self.refresh();
    }

    #[cfg(target_os = "ios")]
    pub fn prepare_ios_voice(&self) -> Result<()> {
        let session = Arc::new(crate::ios::voice::Session::start()?);
        let shared = self.0.shared.clone();
        shared
            .last_capture_ms
            .store(shared.start.elapsed().as_millis() as u64, Relaxed);
        let current = Arc::downgrade(&session);
        *lock(&shared.ios_session) = Some(session);
        let mut previous = shared.capture_frames.load(Relaxed);
        self.0
            .runtime
            .as_ref()
            .expect("audio runtime")
            .spawn(async move {
                for second in 1..=5 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    if !lock(&shared.ios_session)
                        .as_ref()
                        .is_some_and(|s| current.ptr_eq(&Arc::downgrade(s)))
                    {
                        break;
                    }
                    let total = shared.capture_frames.load(Relaxed);
                    tracing::info!(
                        second,
                        frames = total.wrapping_sub(previous),
                        "iOS microphone capture"
                    );
                    previous = total;
                }
            });
        Ok(())
    }

    /// Joined a voice room.
    pub fn start_voice(&self) {
        self.clear_talking();
        self.shared().ai_session.fetch_add(1, Relaxed);
        self.shared().voice.store(true, Relaxed);
        self.refresh();
    }

    /// Drop the old room's audio before queuing its leave cue.
    pub fn end_voice(&self, cue: Option<Cue>) {
        self.clear_talking();
        self.shared().voice.store(false, Relaxed);
        self.pause_phone_withdrawal(false);
        self.shared()
            .phone_withdrawal_requests
            .store(0, std::sync::atomic::Ordering::SeqCst);
        {
            let mut mixer = lock(&self.shared().mixer);
            mixer.tracks.clear();
            *lock(&self.shared().phone) = None;
            mixer.listening.clear();
            mixer.cues.clear();
            mixer.cue_until = None;
        }
        if let Some(cue) = cue {
            self.play_cue(cue);
        }
        #[cfg(target_os = "ios")]
        lock(&self.shared().ios_session).take();
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

    pub fn set_hotkey_down(&self, down: bool) {
        self.shared().hotkey_down.store(down, Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.shared().muted.load(Relaxed)
    }

    /// cpal stops callbacks during an iOS interruption. Keep the person's own mute
    /// setting while showing that the microphone is unavailable.
    pub fn display_muted(&self) -> bool {
        #[cfg(target_os = "ios")]
        {
            let shared = self.shared();
            let last = shared.last_capture_ms.load(Relaxed);
            if shared.voice.load(Relaxed)
                && (shared.start.elapsed().as_millis() as u64).saturating_sub(last) > 1500
            {
                return true;
            }
        }
        self.muted()
    }

    /// Plays the matching chime once per change. Not while deafened: deafening already
    /// mutes, and its own chime covers that.
    pub fn set_muted(&self, on: bool) {
        if on {
            self.clear_talking();
        }
        if self.shared().muted.swap(on, Relaxed) != on && !self.deafened() {
            self.play_cue(if on { Cue::Muted } else { Cue::Unmuted });
        }
    }

    pub fn deafened(&self) -> bool {
        self.shared().deafened.load(Relaxed)
    }

    /// Plays the matching chime once per change, even when deafening.
    pub fn set_deafened(&self, on: bool) {
        if on {
            self.clear_talking();
        }
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
    /// Returns whether the chime was queued.
    pub fn play_cue(&self, cue: Cue) -> bool {
        let settings = lock(&self.shared().settings);
        let prefs = &settings.call_sounds;
        // Deafened people only hear the chimes about deafening itself.
        let silenced = self.deafened() && !matches!(cue, Cue::Deafened | Cue::Undeafened);
        if !prefs.enabled || prefs.volume <= 0.0 || silenced || prefs.disabled.contains(&cue) {
            return false;
        }
        let mut mixer = lock(&self.shared().mixer);
        let now = Instant::now();
        if mixer.cue_until.is_some_and(|until| until <= now) {
            mixer.cues.clear();
        }
        // Keep cues timely if several people join at once or the output stalls.
        if mixer.cues.len() >= RATE as usize / 2 {
            return false;
        }
        mixer.cues.extend(cue.samples(RATE, prefs.volume));
        // A ring is longer, and these two may first have to open the speakers.
        let secs = if matches!(cue, Cue::Ring | Cue::Message) {
            2
        } else {
            1
        };
        mixer.cue_until = Some(now + Duration::from_secs(secs));
        true
    }

    /// Ring for a voice request, also outside a call: that opens the speakers for it.
    pub fn ring(&self) {
        self.play_cue(Cue::Ring);
        self.refresh();
    }

    /// Chime for a new message, also outside a call. False when the sound settings keep it quiet.
    pub fn message(&self) -> bool {
        let played = self.play_cue(Cue::Message);
        self.refresh();
        played
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

    fn clear_talking(&self) {
        self.shared().talked.store(0, Relaxed);
    }

    /// Whether the channel microphone is paused while withdrawing from a phone bridge.
    pub fn channel_mic_paused(&self) -> bool {
        let shared = self.shared();
        shared
            .phone_withdrawal
            .load(std::sync::atomic::Ordering::SeqCst)
            || shared
                .phone_withdrawal_requests
                .load(std::sync::atomic::Ordering::SeqCst)
                > 0
    }

    /// This person is sending sound right now.
    pub fn talking(&self) -> bool {
        let shared = self.shared();
        if self.display_muted() || self.deafened() {
            return false;
        }
        if lock(&shared.settings).push_to_talk
            && !shared.key_down.load(Relaxed)
            && !shared.hotkey_down.load(Relaxed)
        {
            return false;
        }
        let last = shared.talked.load(Relaxed);
        last != 0 && shared.now().saturating_sub(last - 1) < SPEAKING_MS
    }

    #[cfg(test)]
    pub(crate) fn set_talked_for_test(&self, speaking: bool) {
        let value = if speaking {
            self.shared().now().saturating_add(1)
        } else {
            0
        };
        self.shared().talked.store(value, Relaxed);
    }

    /// This person's voice is loud right now; shared apps don't count.
    pub fn speaking(&self, person: &Id) -> bool {
        let now = self.shared().now();
        lock(&self.shared().mixer)
            .tracks
            .values()
            .any(|track| track.stream == (*person, None) && now < track.loud + SPEAKING_MS)
    }

    /// Muted on this device, in the mixer.
    pub fn stream_muted(&self, stream: &StreamId) -> bool {
        lock(&self.shared().settings).mix.level(stream).muted
    }

    pub fn mix(&self) -> crate::call::mixer::Mix {
        lock(&self.shared().settings).mix.clone()
    }

    pub fn set_mix(&self, mix: crate::call::mixer::Mix) {
        let mut settings = self.settings();
        settings.mix = mix;
        self.set_settings(settings);
    }

    /// Start or stop keeping frame levels for calibration; stopping returns them.
    pub fn record(&self, on: bool) -> Option<Vec<f32>> {
        std::mem::replace(&mut *lock(&self.shared().record), on.then(Vec::new))
    }

    /// Play a remote audio track until it ends or is removed.
    pub fn add_track(&self, sid: String, stream: StreamId, track: RtcAudioTrack) {
        let serial = {
            // Settings before the mixer, as everywhere, so a new mix can't slip between.
            let settings = lock(&self.shared().settings);
            let mut mixer = lock(&self.shared().mixer);
            if stream.1.is_some() && !mixer.listening.contains(&sid) {
                return;
            }
            mixer.serial += 1;
            let serial = mixer.serial;
            // Replaces an older reader of the same track, which then stops.
            mixer.tracks.insert(
                sid.clone(),
                Track {
                    serial,
                    gain: settings.mix.gain(&stream),
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

    pub fn stop_listening(&self) {
        let mut mixer = lock(&self.shared().mixer);
        mixer.listening.clear();
        // The local phone bridge is explicitly started by dialing, not remote Listen consent.
        mixer
            .tracks
            .retain(|sid, track| sid == PHONE_TRACK || track.stream.1.is_none());
    }

    pub fn is_listening(&self, sid: &str) -> bool {
        lock(&self.shared().mixer).listening.contains(sid)
    }

    pub fn set_listening(&self, sid: &str, on: bool) {
        let mut mixer = lock(&self.shared().mixer);
        if on {
            mixer.listening.insert(sid.to_owned());
        } else {
            mixer.listening.remove(sid);
            mixer.tracks.remove(sid);
        }
    }

    pub fn remove_track(&self, sid: &str) {
        lock(&self.shared().mixer).tracks.remove(sid);
    }

    pub fn request_phone_withdrawal(&self) {
        self.shared()
            .phone_withdrawal_requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn accept_phone_withdrawal_request(&self) {
        let _ = self.shared().phone_withdrawal_requests.try_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |n| Some(n.saturating_sub(1)),
        );
    }

    /// A privacy interlock, independent of the person's mute/deafen/PTT choices.
    pub fn pause_phone_withdrawal(&self, pause: bool) {
        self.shared()
            .phone_withdrawal
            .store(pause, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn end_phone(&self) {
        lock(&self.shared().mixer).tracks.remove(PHONE_TRACK);
        *lock(&self.shared().phone) = None;
    }

    pub fn phone_consent(&self, allowed: Vec<String>) {
        if let Some(tap) = lock(&self.shared().phone).as_mut() {
            let allowed = allowed.into_iter().collect();
            if tap.allowed != allowed {
                tap.allowed = allowed;
                tap.room.clear();
            }
        }
    }

    /// Bridge a phone call into this call; it plays as `stream`, at its volume.
    pub fn phone_line(&self, stream: StreamId) -> PhoneLine {
        let settings = lock(&self.shared().settings);
        let mut mixer = lock(&self.shared().mixer);
        mixer.serial += 1;
        let serial = mixer.serial;
        mixer.tracks.insert(
            PHONE_TRACK.into(),
            Track {
                serial,
                gain: settings.mix.gain(&stream),
                stream,
                queue: VecDeque::with_capacity(CAP + FRAME),
                primed: false,
                loud: 0,
            },
        );
        *lock(&self.shared().phone) = Some(PhoneTap::default());
        PhoneLine {
            shared: self.0.shared.clone(),
            serial,
        }
    }

    /// Whose a playing track is, once known: a device that joined while it couldn't be
    /// named plays as nobody until then, out of reach of their volume and mute.
    pub fn set_person(&self, sid: &str, person: Id) {
        let settings = lock(&self.shared().settings);
        lock(&self.shared().mixer).set_person(sid, person, &settings.mix);
    }
}

impl Mixer {
    fn set_person(&mut self, sid: &str, person: Id, mix: &crate::call::mixer::Mix) {
        if let Some(track) = self.tracks.get_mut(sid) {
            track.stream.0 = person;
            track.gain = mix.gain(&track.stream);
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
        // Silent history also cannot lower sensitivity when the room sound resumes.
        if target >= AUTO_MIN_DB - AUTO_MARGIN_DB {
            self.floor = target.clamp(self.floor - 2.0, self.floor + NOISE_RISE_DB);
        }
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
    // Declared before the streams so they close before the session on worker exit.
    #[cfg(target_os = "ios")]
    let mut active_session = None;
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
        let want_input = voice || testing;
        #[cfg(target_os = "ios")]
        let want_input = {
            let session = lock(&shared.ios_session).clone();
            let permitted = session.is_some() && !crate::ios::voice::needs_rejoin();
            if permitted {
                active_session = session;
            }
            want_input && permitted
        };
        let want_input = want_input.then_some(settings.input);
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
        #[cfg(target_os = "ios")]
        let want_output = want_output.filter(|_| !crate::ios::voice::needs_rejoin());
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
        #[cfg(target_os = "ios")]
        if input.is_none() && output.is_none() {
            active_session.take();
        }
        retry = problem.is_some();
        *lock(&shared.problem) = problem.map(str::to_owned);
    }
}

fn save(shared: &Shared) {
    let file = lock(&shared.file);
    let Some(path) = file.as_ref() else { return };
    let dir = path.parent().unwrap_or(Path::new("."));
    if let Err(error) = save_json(dir, path, &shared.settings()) {
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
    #[cfg(target_os = "ios")]
    anyhow::ensure!(config.channels > 0, "microphone has no input channels");
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    let shared = shared.clone();
    Ok(device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            #[cfg(target_os = "ios")]
            {
                shared
                    .capture_frames
                    .fetch_add((data.len() / channels) as u32, Relaxed);
                shared
                    .last_capture_ms
                    .store(shared.start.elapsed().as_millis() as u64, Relaxed);
            }
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
    let mut phone = lock(&shared.phone);
    mix_tracks(&mut mixer, shared.deafened.load(Relaxed), phone.as_mut())
}

/// Also queues the remote tracks, at full volume, for a bridged phone call: it hears the
/// room even when this person turned someone down or deafened.
fn mix_tracks(mixer: &mut Mixer, deafened: bool, phone: Option<&mut PhoneTap>) -> [i16; FRAME] {
    let mut sum = [0f32; FRAME];
    let allowed = phone.as_ref().map(|tap| &tap.allowed);
    let mut room = phone.is_some().then_some([0f32; FRAME]);
    let Mixer {
        tracks,
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
    for (sid, track) in tracks.iter_mut() {
        let gain = if deafened { 0.0 } else { track.gain };
        let mut room = room.as_mut().filter(|_| {
            track.stream.1.is_none() && allowed.is_some_and(|allowed| allowed.contains(sid))
        });
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
        for (at, slot) in sum.iter_mut().enumerate() {
            let Some(sample) = track.queue.pop_front() else {
                track.primed = false;
                break;
            };
            *slot += f32::from(sample) * gain;
            if let Some(room) = room.as_deref_mut() {
                room[at] += f32::from(sample);
            }
        }
    }
    let clamp = |s: f32| s.clamp(-32768.0, 32767.0) as i16;
    if let (Some(tap), Some(room)) = (phone, room) {
        PhoneTap::push(&mut tap.room, room.map(clamp));
    }
    sum.map(clamp)
}

struct VoiceGain {
    learned: f32,
    applied: f32,
}

impl Default for VoiceGain {
    fn default() -> Self {
        Self {
            learned: 1.0,
            applied: 1.0,
        }
    }
}

impl VoiceGain {
    fn apply(&mut self, frame: &mut [i16; FRAME], db: f32, threshold: f32) {
        if db > threshold {
            let target = 10f32.powf((-18.0 - db) / 20.0).clamp(0.25, 4.0);
            self.learned +=
                (target - self.learned) * if target < self.learned { 0.25 } else { 0.005 };
        }
        self.ramp(frame);
    }

    fn reset(&mut self, frame: &mut [i16; FRAME]) {
        self.learned = 1.0;
        if self.applied != 1.0 {
            self.ramp(frame);
        }
    }

    fn ramp(&mut self, frame: &mut [i16; FRAME]) {
        let peak = frame
            .iter()
            .map(|x| x.unsigned_abs())
            .max()
            .unwrap_or(1)
            .max(1);
        let end = self.learned.min(32767.0 / f32::from(peak));
        for (i, sample) in frame.iter_mut().enumerate() {
            self.applied += (end - self.applied) / (FRAME - i) as f32;
            // A transient can reach full scale before the ramp has finished.
            self.applied = self
                .applied
                .min(32767.0 / f32::from(sample.unsigned_abs().max(1)));
            *sample = (f32::from(*sample) * self.applied) as i16;
        }
    }
}

/// Probe after 5, 10, 20 and 40 seconds of steady capture. After four failed
/// probes stay dry until a new call or an explicit change to the AI switch.
#[derive(Default)]
struct Recovery {
    attempts: u32,
    since: Option<Instant>,
    frames: u32,
    clean: u32,
    paused: bool,
}

impl Recovery {
    fn fail(&mut self, now: Instant) {
        self.paused = true;
        self.since = Some(now);
        self.frames = 0;
        self.clean = 0;
    }

    fn ready(&mut self, now: Instant) -> bool {
        let Some(start) = self.since else {
            return false;
        };
        if self.attempts >= 4 {
            return false;
        }
        let seconds = 5 << self.attempts;
        self.frames += 1;
        if self.frames < 100 * seconds {
            return false;
        }
        self.frames = 0;
        let millis = u128::from(seconds) * 1000;
        if now.duration_since(start).as_millis().abs_diff(millis) > 200 {
            self.since = Some(now);
            return false;
        }
        self.since = None;
        self.attempts += 1;
        true
    }

    fn confirmed(&mut self, clean: bool) -> bool {
        if !self.paused || self.since.is_some() {
            return false;
        }
        self.clean = if clean { self.clean + 1 } else { 0 };
        if self.clean < 200 {
            return false;
        }
        self.paused = false;
        true
    }
}

/// The microphone thread: resample, process, gate, and hand frames to the source.
fn process(shared: &Shared, frames: &Receiver<Mic>) {
    let mut built = u64::MAX;
    let mut apm = None;
    let mut apm_config = None;
    let mut session = 0;
    let mut denoiser: Option<Worker> = None;
    let mut loading: Option<thread::JoinHandle<Result<denoise::Denoiser>>> = None;
    let mut ai_enabled = false;
    let mut recovery = Recovery::default();
    let mut mix = denoise::Mix::default();
    let mut gain = VoiceGain::default();
    let mut framer = Framer::default();
    let mut gate = Gate::default();
    let mut noise = NoiseFloor::default();
    let mut check = mic_check::Detector::default();
    while let Ok(message) = frames.recv() {
        let wanted = shared.processing.load(Relaxed);
        let current_session = shared.ai_session.load(Relaxed);
        if wanted != built || current_session != session {
            if current_session != session {
                session = current_session;
                recovery = Recovery::default();
                shared.ai_failed.store(false, Relaxed);
            }
            let s = shared.settings();
            built = wanted;
            ai_enabled = s.ai_noise;
            if !ai_enabled {
                denoiser = None;
                shared.ai_failed.store(false, Relaxed);
            } else if denoiser.is_none()
                && loading.is_none()
                && !recovery.paused
                && *lock(&shared.model) == Model::Ready
            {
                let file = shared.model_file.clone();
                loading = Some(thread::spawn(move || denoise::load(&file)));
            }
            // Keep the echo canceller and classic suppression running unchanged while
            // a replacement model proves it can meet deadlines.
            let use_ai = denoiser.is_some() && !recovery.paused;
            let config = (s.echo_cancellation, s.noise_suppression && !use_ai);
            if apm_config != Some(config) {
                apm = Some(build_apm(&s, use_ai));
                apm_config = Some(config);
            }
        }
        if loading.as_ref().is_some_and(|job| job.is_finished()) {
            match loading
                .take()
                .unwrap()
                .join()
                .unwrap_or_else(|_| Err(anyhow!("noise worker panicked")))
            {
                Ok(_) if !ai_enabled => (),
                Ok(model) => match Worker::start(model) {
                    Ok(worker) => {
                        denoiser = Some(worker);
                        if !recovery.paused {
                            shared.ai_failed.store(false, Relaxed);
                            built = u64::MAX;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "starting AI noise removal worker failed");
                        recovery.fail(Instant::now());
                        shared.ai_failed.store(true, Relaxed);
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, "loading AI noise removal failed");
                    shared.ai_failed.store(ai_enabled, Relaxed);
                    if let Err(error) = fs::remove_file(&shared.model_file) {
                        tracing::warn!(%error, "removing unusable noise model failed");
                    }
                    *lock(&shared.model) = Model::Missing;
                    recovery = Recovery::default();
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
            let (push_to_talk, manual_threshold, watch, auto_gain, strength) = {
                let s = lock(&shared.settings);
                (
                    s.push_to_talk,
                    (!s.auto_threshold).then_some(s.threshold),
                    s.mic_hint && !s.ai_noise,
                    s.auto_gain,
                    s.noise_strength,
                )
            };
            let clean = denoiser
                .as_mut()
                .and_then(|worker| worker.process(&frame, strength));
            let paused = recovery.paused;
            // Probe silently; keep the dry path and its notice until two clean seconds.
            let confirmed = recovery.confirmed(clean.is_some());
            mix.apply(
                &mut frame,
                clean.filter(|_| !paused),
                denoiser.is_some() || paused,
            );
            if confirmed {
                shared.ai_failed.store(false, Relaxed);
                built = u64::MAX;
            }
            if denoiser.as_ref().is_some_and(Worker::overloaded) {
                tracing::warn!("AI noise removal paused after missed audio deadlines");
                denoiser = None;
                recovery.fail(Instant::now());
                shared.ai_failed.store(true, Relaxed);
                built = u64::MAX;
            }
            if recovery.ready(Instant::now()) {
                let file = shared.model_file.clone();
                loading = Some(thread::spawn(move || denoise::load(&file)));
            }
            let db = level_db(&frame);
            shared.level.store(db.to_bits(), Relaxed);
            if let Some(levels) = lock(&shared.record).as_mut() {
                levels.push(db);
            }
            noise.push(db);
            let auto = noise.threshold();
            shared.auto_threshold.store(auto.to_bits(), Relaxed);
            let threshold = manual_threshold.unwrap_or(auto);
            if !shared.voice.load(Relaxed) {
                // Out of a call: the next one is judged afresh.
                check = mic_check::Detector::default();
            } else if watch {
                let muted = shared.muted.load(Relaxed) || shared.deafened.load(Relaxed);
                let frame = mic_check::Frame {
                    // The gate holds its learned floor through silence; the nudge needs actual noise.
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
                    shared.key_down.load(Relaxed) || shared.hotkey_down.load(Relaxed)
                } else {
                    voiced
                };
            if open && db > SILENT_DB {
                shared.talked.store(shared.now().saturating_add(1), Relaxed);
            }
            if auto_gain {
                gain.apply(&mut frame, db, threshold);
            } else {
                gain.reset(&mut frame);
            }
            if !open {
                frame.fill(0);
            }
            if shared.voice.load(Relaxed) {
                let outgoing = channel_microphone(shared, &frame);
                send(&shared.source, &outgoing);
                if let Some(tap) = lock(&shared.phone).as_mut() {
                    PhoneTap::push(&mut tap.mic, frame);
                }
            }
        }
    }
}

fn channel_microphone(shared: &Shared, frame: &[i16; FRAME]) -> [i16; FRAME] {
    if shared
        .phone_withdrawal
        .load(std::sync::atomic::Ordering::SeqCst)
        || shared
            .phone_withdrawal_requests
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
    {
        [0; FRAME]
    } else {
        *frame
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
            gain: 1.0,
            stream,
            queue: VecDeque::new(),
            primed: false,
            loud: 0,
        };
        let app = "Spotify".to_owned();
        mixer.tracks.insert("voice".into(), track(([0; 32], None)));
        mixer
            .tracks
            .insert("app".into(), track(([0; 32], Some(app.clone()))));
        // Once known, the track plays at that person's volume.
        let mut mix = crate::call::mixer::Mix::default();
        let mut ann = mix.person(&[7; 32]);
        ann.voice.muted = true;
        mix.set_person(&[7; 32], ann);
        mixer.set_person("voice", [7; 32], &mix);
        mixer.set_person("app", [7; 32], &mix);
        mixer.set_person("gone", [7; 32], &mix);
        assert_eq!(mixer.tracks["voice"].stream, ([7; 32], None));
        assert_eq!(mixer.tracks["app"].stream, ([7; 32], Some(app)));
        assert_eq!(mixer.tracks["voice"].gain, 0.0);
        assert_eq!(mixer.tracks["app"].gain, 1.0);
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
        feed(&mut noise, &mut gate, -55.0, 5.0);
        assert!(noise.threshold() < -40.0 + AUTO_MARGIN_DB - 5.0);
        feed(&mut noise, &mut gate, -55.0, 4.0);
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
    fn quiet_speech_opens_and_receives_gain() {
        let (mut noise, mut gate) = (NoiseFloor::default(), Gate::default());
        feed(&mut noise, &mut gate, -90.0, 120.0);
        let mut gain = VoiceGain::default();
        for _ in 0..30 {
            let mut speech = std::array::from_fn(|i| {
                (147.0 * (std::f32::consts::TAU * i as f32 / 120.0).sin()) as i16
            });
            let db = level_db(&speech);
            assert!((db + 50.0).abs() < 0.2);
            noise.push(db);
            assert!(gate.update(db, noise.threshold()));
            gain.apply(&mut speech, db, noise.threshold());
        }
        let learned = gain.learned;
        let mut tail = [25; FRAME];
        gain.apply(&mut tail, -62.0, noise.threshold());
        assert_eq!(gain.learned, learned);
        assert!(tail.iter().all(|s| *s > 25));
    }

    #[test]
    fn turning_off_gain_ramps_back_to_unity_in_one_hop() {
        let mut gain = VoiceGain {
            learned: 4.0,
            applied: 4.0,
        };
        let mut frame = [1000; FRAME];
        gain.reset(&mut frame);
        assert!(frame[0] > 3900);
        assert!(frame.windows(2).all(|w| w[0] >= w[1] && w[0] - w[1] <= 7));
        assert_eq!(frame[FRAME - 1], 1000);
    }

    #[test]
    fn recovery_backs_off_and_stops_after_four_probes() {
        let mut now = Instant::now();
        let mut retry = Recovery::default();
        for seconds in [5, 10, 20, 40] {
            retry.fail(now);
            for i in 1..=seconds * 100 {
                now += Duration::from_millis(10);
                assert_eq!(retry.ready(now), i == seconds * 100);
            }
            assert!(retry.paused);
        }
        retry.fail(now);
        for _ in 0..10000 {
            now += Duration::from_millis(10);
            assert!(!retry.ready(now));
        }
        assert!(retry.paused);
    }

    #[test]
    fn retry_needs_two_uninterrupted_clean_seconds() {
        let mut retry = Recovery {
            attempts: 1,
            paused: true,
            ..Default::default()
        };
        for _ in 0..199 {
            assert!(!retry.confirmed(true));
        }
        assert!(!retry.confirmed(false));
        for _ in 0..199 {
            assert!(!retry.confirmed(true));
        }
        assert!(retry.paused);
        assert!(retry.confirmed(true));
        assert!(!retry.paused);
        assert_eq!(retry.attempts, 1);
    }

    #[test]
    fn vacuum_stops_and_quiet_speech_opens_within_two_seconds() {
        let (mut noise, mut gate) = (NoiseFloor::default(), Gate::default());
        feed(&mut noise, &mut gate, -35.0, 30.0);
        assert_eq!(noise.threshold(), AUTO_MAX_DB);
        feed(&mut noise, &mut gate, -68.0, 2.0);
        assert!(gate.update(-50.0, noise.threshold()));
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
            let previous = gain.learned;
            gain.apply(&mut frame, db, noise.threshold());
            assert!(gain.learned <= 4.0);
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
                assert!(
                    !voiced,
                    "frame {i}: {db} dBFS, threshold {}, initial {initial}",
                    noise.threshold()
                );
                assert_eq!(previous, gain.learned);
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
    fn shared_playback_requires_consent_and_stop_discards_buffered_samples() {
        let data = tempfile::tempdir().unwrap();
        let audio = Audio::new(data.path()).unwrap();
        let source = NativeAudioSource::new(AudioSourceOptions::default(), RATE, 1, 0);
        let track = livekit::prelude::LocalAudioTrack::create_audio_track(
            "synthetic",
            livekit::webrtc::audio_source::RtcAudioSource::Native(source),
        );
        let shared = ([1; 32], Some("Music".to_owned()));
        audio.add_track("TR_app".into(), shared.clone(), track.rtc_track());
        assert!(lock(&audio.shared().mixer).tracks.is_empty());
        audio.set_listening("TR_app", true);
        audio.add_track("TR_app".into(), shared, track.rtc_track());
        audio.add_track("TR_mic".into(), ([1; 32], None), track.rtc_track());
        {
            let mut mixer = lock(&audio.shared().mixer);
            for t in mixer.tracks.values_mut() {
                t.queue.extend(std::iter::repeat_n(1000, PRIME * 2));
            }
            assert!(mix_tracks(&mut mixer, false, None).iter().any(|&v| v != 0));
        }
        audio.set_listening("TR_app", false);
        let mut mixer = lock(&audio.shared().mixer);
        assert!(!mixer.tracks.contains_key("TR_app"));
        assert!(
            mix_tracks(&mut mixer, false, None).iter().any(|&v| v != 0),
            "microphone continues"
        );
        assert!(
            mix_tracks(&mut mixer, true, None).iter().all(|&v| v == 0),
            "deafen still applies"
        );
        drop(mixer);
        let line = audio.phone_line(([1; 32], Some("Phone".to_owned())));
        audio.stop_listening();
        assert!(lock(&audio.shared().mixer).tracks.contains_key("TR_mic"));
        assert!(lock(&audio.shared().mixer).tracks.contains_key(PHONE_TRACK));
        drop(line);
        assert!(lock(&audio.shared().phone).is_none());
        let line = audio.phone_line(([1; 32], Some("Phone".to_owned())));
        audio.end_voice(None);
        assert!(lock(&audio.shared().phone).is_none());
        let mut samples = [1; FRAME];
        line.take(&mut samples);
        assert!(samples.iter().all(|&sample| sample == 0));
    }

    #[test]
    fn mixer_consumes_cues_and_still_plays_them_while_deafened() {
        let samples: VecDeque<_> = Cue::YouJoined.samples(RATE, 0.5).collect();
        let mut mixer = Mixer {
            cues: samples.clone(),
            ..Mixer::default()
        };
        assert!(
            mix_tracks(&mut mixer, false, None)
                .iter()
                .any(|&sample| sample != 0)
        );
        assert_eq!(mixer.cues.len(), samples.len() - FRAME);
        // Deafening silences voices, not the chime that confirms it.
        assert!(
            mix_tracks(&mut mixer, true, None)
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
        let mut mix = crate::call::mixer::Mix::default();
        let mut person = mix.person(&quiet.0);
        person.voice.volume = 0.25;
        mix.set_person(&quiet.0, person);
        let track = |stream: StreamId, sample| Track {
            serial: 0,
            gain: mix.gain(&stream),
            stream,
            queue: VecDeque::from(vec![sample; PRIME]),
            primed: false,
            loud: 0,
        };
        mixer.tracks.insert("a".into(), track(loud.clone(), 8000));
        let alone = mix_tracks(&mut mixer, false, None);
        assert!(alone.iter().all(|&s| s == 8000));

        mixer.tracks.clear();
        mixer.tracks.insert("b".into(), track(quiet, 8000));
        let quarter = mix_tracks(&mut mixer, false, None);
        assert!(quarter.iter().all(|&s| s == 2000));

        assert!(mix_tracks(&mut mixer, true, None).iter().all(|&s| s == 0));
    }

    #[test]
    fn withdrawal_interlock_stops_pcm_before_signaling_without_changing_user_mute() {
        let data = tempfile::tempdir().unwrap();
        let audio = Audio::new(data.path()).unwrap();
        let speech = [1200; FRAME];
        let initially_muted = audio.muted();
        assert_eq!(channel_microphone(audio.shared(), &speech), speech);
        audio.request_phone_withdrawal();
        // An already-running network refresh must not undo a queued UI request.
        audio.pause_phone_withdrawal(false);
        assert_eq!(channel_microphone(audio.shared(), &speech), [0; FRAME]);
        audio.pause_phone_withdrawal(true);
        audio.accept_phone_withdrawal_request();
        assert_eq!(channel_microphone(audio.shared(), &speech), [0; FRAME]);
        // Failed publication or missing bridge acknowledgement leaves this interlock set.
        assert_eq!(audio.muted(), initially_muted);
        audio.pause_phone_withdrawal(false);
        assert_eq!(channel_microphone(audio.shared(), &speech), speech);
        // The gate never invents sound when mute, deafen or PTT already supplied silence.
        assert_eq!(channel_microphone(audio.shared(), &[0; FRAME]), [0; FRAME]);
        audio.request_phone_withdrawal();
        assert!(audio.channel_mic_paused());
        audio.end_voice(None);
        assert!(!audio.channel_mic_paused());
        assert_eq!(channel_microphone(audio.shared(), &speech), speech);
    }

    #[test]
    fn talking_respects_startup_mute_deafen_ptt_and_call_end() {
        let data = tempfile::tempdir().unwrap();
        let audio = Audio::new(data.path()).unwrap();
        let mut settings = audio.settings();
        settings.call_sounds.enabled = false;
        audio.set_settings(settings.clone());

        // Never falsely lit right after start before any sound arrives.
        assert!(!audio.talking());

        // Voice activation: recent transmitted speech lights up.
        audio.set_talked_for_test(true);
        assert!(audio.talking());

        // Muting clears talking immediately.
        audio.set_muted(true);
        assert!(!audio.talking());
        audio.set_talked_for_test(true);
        assert!(!audio.talking());
        audio.set_muted(false);
        audio.set_talked_for_test(true);
        assert!(audio.talking());

        // Deafening clears talking immediately.
        audio.set_deafened(true);
        assert!(!audio.talking());
        audio.set_talked_for_test(true);
        assert!(!audio.talking());
        audio.set_deafened(false);

        // Push-to-talk requires key_down or hotkey_down in addition to speech.
        settings.push_to_talk = true;
        audio.set_settings(settings);
        audio.set_talked_for_test(true);
        assert!(!audio.talking());
        audio.set_key_down(true);
        assert!(audio.talking());
        audio.set_key_down(false);
        assert!(!audio.talking());
        audio.set_hotkey_down(true);
        assert!(audio.talking());
        audio.set_hotkey_down(false);
        assert!(!audio.talking());

        // Ending voice clears any lingering speech timestamp.
        audio.set_hotkey_down(true);
        audio.set_talked_for_test(true);
        assert!(audio.talking());
        audio.end_voice(None);
        assert!(!audio.talking());
    }

    #[test]
    fn phone_export_needs_consent_and_excludes_shared_audio() {
        let track = |app| Track {
            serial: 0,
            gain: 1.0,
            stream: ([1; 32], app),
            queue: VecDeque::from(vec![1000; PRIME * 4]),
            primed: false,
            loud: 0,
        };
        let mut mixer = Mixer::default();
        mixer.tracks.insert("voice".into(), track(None));
        mixer.tracks.insert("newcomer".into(), track(None));
        mixer
            .tracks
            .insert("app".into(), track(Some("music".into())));
        let mut tap = PhoneTap::default();
        mix_tracks(&mut mixer, false, Some(&mut tap));
        assert!(tap.room.drain(..).all(|sample| sample == 0));
        tap.allowed.extend(["voice".into(), "app".into()]);
        mix_tracks(&mut mixer, true, Some(&mut tap));
        assert!(tap.room.drain(..).all(|sample| sample == 1000));
        tap.allowed.clear();
        mix_tracks(&mut mixer, false, Some(&mut tap));
        assert!(tap.room.drain(..).all(|sample| sample == 0));
    }

    #[test]
    fn a_phone_hears_the_room_but_not_itself() {
        let track = |gain, sample| Track {
            serial: 0,
            gain,
            stream: ([1; 32], None),
            queue: VecDeque::from(vec![sample; PRIME]),
            primed: false,
            loud: 0,
        };
        let mut mixer = Mixer::default();
        // Turned down here, but the phone hears it at full volume.
        mixer.tracks.insert("voice".into(), track(0.25, 4000));
        mixer.tracks.insert(PHONE_TRACK.into(), track(1.0, 1000));
        let mut tap = PhoneTap::default();
        tap.allowed.insert("voice".into());
        let speakers = mix_tracks(&mut mixer, false, Some(&mut tap));
        assert!(
            speakers.iter().all(|&s| s == 2000),
            "the voice and the phone play here"
        );
        assert_eq!(tap.room.len(), FRAME);
        assert!(
            tap.room.iter().all(|&s| s == 4000),
            "the phone never hears itself"
        );
        // Deafened, this person hears nothing, the phone still hears the room.
        let speakers = mix_tracks(&mut mixer, true, Some(&mut tap));
        assert!(speakers.iter().all(|&s| s == 0));
        assert!(tap.room.iter().all(|&s| s == 4000));
        // It never lags more than PHONE_LAG behind.
        PhoneTap::push(&mut tap.mic, vec![1; 2 * PHONE_LAG]);
        assert_eq!(tap.mic.len(), PHONE_LAG);
    }
}
