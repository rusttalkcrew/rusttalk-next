//! What winit 0.30 leaves out on Android (docs/mobile.md): typed text, the space
//! the keyboard and system bars cover, and ending with the activity.
//!
//! The on-screen keyboard doesn't send keys: it edits a text buffer that
//! GameActivity's GameTextInput keeps, and winit drops the events that say it
//! changed. The buffer holds a stretch of the focused egui field around its cursor
//! (dots for secrets), so the keyboard can delete and correct what is there; each
//! change the keyboard makes to it becomes the same change, at the same place, in
//! the field. Moving the cursor in the field starts the buffer over there.

#[cfg(target_os = "android")]
pub use activity::*;

use std::fmt::{self, Write as _};

use eframe::egui::{
    Context, Event, Id, Key, Modifiers,
    text::{CCursor, CCursorRange},
    text_edit::TextEditState,
};
use zeroize::{Zeroize, Zeroizing};

/// GameTextInput's native text buffer (`DEFAULT_MAX_STRING_SIZE`), which never
/// shrinks. It copies at most this minus the closing NUL, but keeps reporting the
/// full length of a longer text.
const NATIVE_BUFFER: usize = 1 << 16;
/// The keyboard's buffer is capped in UTF-16 units (Java's `LengthFilter`) so its
/// modified UTF-8 (at most 3 bytes a unit) always fits the native buffer.
const MAX_TYPED: i32 = ((NATIVE_BUFFER - 1) / 3) as i32;
/// Characters of the field on each side of the cursor that the buffer holds.
const CONTEXT: usize = 100;
/// The most characters a buffer starts with (at most 2 UTF-16 units each), well
/// under `MAX_TYPED`; a longer selection starts with an empty buffer instead.
const MAX_SEED: usize = 1000;

/// How many bytes of a reported text length GameTextInput really copied.
fn copied(length: i32) -> usize {
    usize::try_from(length).unwrap_or(0).min(NATIVE_BUFFER - 1)
}

/// Modified UTF-8 cut to whole characters: a copy cut short can end inside a
/// sequence, or between the two halves of a surrogate pair.
fn whole_chars(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    if let Some(lead) = (0..end).rev().find(|&i| bytes[i] & 0xc0 != 0x80) {
        let len = match bytes[lead] {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 1,
        };
        if end - lead < len {
            end = lead;
        }
    }
    // A high surrogate (U+D800..U+DBFF) waits for its low half.
    if end >= 3 && bytes[end - 3] == 0xed && (0xa0..=0xaf).contains(&bytes[end - 2]) {
        end -= 3;
    }
    &bytes[..end]
}

/// The keyboard's buffer: its text, and the selection in characters (sorted).
#[derive(Clone, Default, PartialEq)]
pub struct Buffer {
    pub text: Zeroizing<String>,
    pub selection: (usize, usize),
}

impl Buffer {
    /// From Java's modified UTF-8, with the selection in UTF-16 units as Java
    /// counts them. Every copy of the text is wiped when dropped.
    pub fn from_java(bytes: &[u8], selection: (i32, i32)) -> Self {
        let units = Zeroizing::new(utf16_units(bytes));
        // Room for the longest result, so the string never moves and leaves a copy.
        let mut text = Zeroizing::new(String::with_capacity(units.len() * 3));
        let mut starts = Vec::with_capacity(units.len());
        let mut unit = 0;
        for c in char::decode_utf16(units.iter().copied()) {
            let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
            starts.push(unit);
            unit += c.len_utf16();
            text.push(c);
        }
        // No selection (-1) means the end.
        let at = |offset: i32| {
            usize::try_from(offset).map_or(starts.len(), |o| starts.partition_point(|&s| s < o))
        };
        let (a, b) = (at(selection.0), at(selection.1));
        Self {
            text,
            selection: (a.min(b), a.max(b)),
        }
    }

    /// To Java's modified UTF-8 ending in NUL, with the selection in UTF-16 units.
    pub fn to_java(&self) -> (Zeroizing<Vec<u8>>, (i32, i32)) {
        // At most 6 bytes a character, so the vector never moves either.
        let mut bytes = Zeroizing::new(Vec::with_capacity(self.text.len() * 2 + 1));
        let (mut units, mut selection) = (0, (0, 0));
        let mut count = 0;
        for (index, c) in self.text.chars().enumerate() {
            if index == self.selection.0 {
                selection.0 = units;
            }
            if index == self.selection.1 {
                selection.1 = units;
            }
            let mut pair = [0; 2];
            for &unit in c.encode_utf16(&mut pair).iter() {
                push_modified_utf8(&mut bytes, unit);
            }
            units += c.len_utf16();
            count = index + 1;
        }
        if self.selection.0 >= count {
            selection.0 = units;
        }
        if self.selection.1 >= count {
            selection.1 = units;
        }
        bytes.push(0);
        let as_i32 = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
        (bytes, (as_i32(selection.0), as_i32(selection.1)))
    }
}

fn push_modified_utf8(bytes: &mut Vec<u8>, unit: u16) {
    // Java writes NUL as two bytes and each surrogate on its own.
    match unit {
        0x01..=0x7f => bytes.push(unit as u8),
        0x00 | 0x80..=0x7ff => bytes.extend([0xc0 | (unit >> 6) as u8, 0x80 | (unit & 0x3f) as u8]),
        _ => bytes.extend([
            0xe0 | (unit >> 12) as u8,
            0x80 | ((unit >> 6) & 0x3f) as u8,
            0x80 | (unit & 0x3f) as u8,
        ]),
    }
}

/// Modified UTF-8 to UTF-16 units; a broken sequence becomes U+FFFD.
fn utf16_units(bytes: &[u8]) -> Vec<u16> {
    let mut units = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let lead = bytes[i];
        let len = match lead {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            _ => 0,
        };
        let tail = bytes.get(i + 1..i + len.max(1)).unwrap_or_default();
        if len == 0 || tail.len() + 1 != len || tail.iter().any(|b| b & 0xc0 != 0x80) {
            units.push(0xfffd);
            i += 1;
            continue;
        }
        let first = if len == 1 { lead } else { lead & (0x7f >> len) };
        units.push(
            tail.iter()
                .fold(u16::from(first), |unit, b| unit << 6 | u16::from(b & 0x3f)),
        );
        i += len;
    }
    units
}

/// The field's text around its cursor, which the buffer starts with.
#[derive(Clone, Default, PartialEq)]
pub struct Seed {
    pub text: Zeroizing<String>,
    /// Where `text` starts in the field, in characters.
    pub start: usize,
}

impl Seed {
    /// Up to `CONTEXT` characters each side of `cursor`, at most `MAX_SEED` in
    /// all; empty for a longer selection. Never for a secret field.
    pub fn around(text: &str, cursor: (usize, usize)) -> Self {
        let len = text.chars().count();
        let (from, to) = (cursor.0.min(len), cursor.1.min(len));
        let Some(room) = MAX_SEED.checked_sub(to - from) else {
            return Self::default();
        };
        let context = CONTEXT.min(room / 2);
        let start = from.saturating_sub(context);
        let end = (to + context).min(len);
        let mut seed = Zeroizing::new(String::with_capacity((end - start) * 4));
        seed.extend(text.chars().skip(start).take(end - start));
        Self { text: seed, start }
    }
}

/// What the focused field shares with the keyboard (see `share_field`).
#[derive(Clone)]
struct SharedField {
    field: Id,
    cursor: (usize, usize),
    /// Characters in the field.
    len: usize,
    secret: bool,
    /// Empty for a secret.
    seed: Seed,
}

fn shared_id() -> Id {
    Id::unique("keyboard field")
}

/// The focused field shares its cursor and length, and unless it is `secret` its
/// text around the cursor (`ui::share_with_keyboard`). Being secret is known before
/// any text is looked at, so a secret never reaches the keyboard.
pub fn share_field(
    ctx: &Context,
    field: Id,
    text: &str,
    cursor: Option<CCursorRange>,
    secret: bool,
) {
    let Some(cursor) = cursor.map(sorted) else {
        return;
    };
    let seed = match secret {
        true => Seed::default(),
        false => Seed::around(text, cursor),
    };
    let len = text.chars().count();
    let shared = SharedField {
        field,
        cursor,
        len,
        secret,
        seed,
    };
    ctx.data_mut(|data| data.insert_temp(shared_id(), shared));
}

/// Whether `field` shared itself.
#[cfg(target_os = "android")]
pub fn has_shared(ctx: &Context, field: Id) -> bool {
    ctx.data(|data| data.get_temp::<SharedField>(shared_id()))
        .is_some_and(|shared| shared.field == field)
}

/// Forget what the last field shared, unless it is `keep` (the newly focused one).
#[cfg(target_os = "android")]
pub fn forget_shared(ctx: &Context, keep: Option<Id>) {
    if !keep.is_some_and(|field| has_shared(ctx, field)) {
        ctx.data_mut(|data| data.remove::<SharedField>(shared_id()));
    }
}

/// Start the buffer over for `field`, and where the field's cursor goes. A
/// normal field's buffer starts with its text around the cursor; a secret field's
/// starts empty, and on focus with the cursor at the end, as Android password
/// fields do (Backspace deletes from the end).
pub fn start(
    ctx: &Context,
    bridge: &mut Bridge,
    field: Id,
    cursor: (usize, usize),
    on_focus: bool,
) -> (usize, usize) {
    let shared = ctx
        .data(|data| data.get_temp::<SharedField>(shared_id()))
        .filter(|shared| shared.field == field);
    let (cursor, seed) = match shared {
        Some(shared) if shared.secret && on_focus => ((shared.len, shared.len), Seed::default()),
        Some(shared) if !shared.secret && shared.cursor == cursor => (cursor, shared.seed),
        _ => (cursor, Seed::default()),
    };
    bridge.start(cursor, seed);
    cursor
}

/// Turns changes of the keyboard's buffer into the same changes of the focused field.
#[derive(Default)]
pub struct Bridge {
    /// The buffer as last applied.
    last: Buffer,
    /// Where the buffer starts in the field, and how many of the field's characters
    /// it stands for (with an empty buffer: the selection its first change replaces).
    anchor: usize,
    region: usize,
    /// The field's cursor after the last change.
    expected: Option<(usize, usize)>,
}

/// What [`Bridge::update`] asks for.
#[derive(Debug, Default, PartialEq)]
pub struct Change {
    /// Put the field's cursor here before this frame's events.
    pub cursor: Option<(usize, usize)>,
    /// Start the buffer over at the field's cursor.
    pub restart: bool,
}

impl Bridge {
    /// Start the buffer over with `seed`, if it holds the cursor; empty otherwise.
    pub fn start(&mut self, cursor: (usize, usize), seed: Seed) {
        let len = seed.text.chars().count();
        *self = match seed.start <= cursor.0 && cursor.1 <= seed.start + len {
            true => Self {
                last: Buffer {
                    selection: (cursor.0 - seed.start, cursor.1 - seed.start),
                    text: seed.text,
                },
                anchor: seed.start,
                region: len,
                expected: Some(cursor),
            },
            false => Self {
                anchor: cursor.0,
                region: cursor.1 - cursor.0,
                expected: Some(cursor),
                ..Self::default()
            },
        };
    }

    /// What the keyboard's buffer should hold now.
    pub fn buffer(&self) -> &Buffer {
        &self.last
    }

    pub fn update(
        &mut self,
        cursor: (usize, usize),
        buffer: &Buffer,
        events: &mut Vec<Event>,
    ) -> Change {
        if self.expected != Some(cursor) {
            // The cursor moved in the field: a tap, a key, or the app set the text.
            return Change {
                restart: true,
                ..Change::default()
            };
        }
        if buffer.text == self.last.text {
            if buffer.selection == self.last.selection {
                return Change::default();
            }
            let at = |c: usize| self.anchor + c.min(self.region);
            let cursor = (at(buffer.selection.0), at(buffer.selection.1));
            self.last.selection = buffer.selection;
            self.expected = Some(cursor);
            return Change {
                cursor: Some(cursor),
                restart: false,
            };
        }
        let (old, new) = (&*self.last.text, &*buffer.text);
        let (old_len, new_len) = (old.chars().count(), new.chars().count());
        let same = |(a, b): &(char, char)| a == b;
        let prefix = old.chars().zip(new.chars()).take_while(same).count();
        let suffix = (old
            .chars()
            .rev()
            .zip(new.chars().rev())
            .take_while(same)
            .count())
        .min(old_len.min(new_len) - prefix);
        let start = self.anchor + prefix;
        let end = (self.anchor + self.region)
            .saturating_sub(suffix)
            .max(start);
        let added = new_len - prefix - suffix;
        let inserted: String = new.chars().skip(prefix).take(added).collect();
        if inserted.contains('\n') {
            // Newlines are text (egui drops a lone one as `Text`); only the
            // keyboard's action key is Enter.
            events.push(Event::Paste(inserted));
        } else if !inserted.is_empty() {
            events.push(Event::Text(inserted));
        } else if start < end {
            events.push(Event::Key {
                key: Key::Delete,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            });
        }
        self.region = new_len;
        self.last.text = buffer.text.clone();
        // Where the field's cursor ends up; a different selection follows next frame.
        self.last.selection = (prefix + added, prefix + added);
        self.expected = Some((start + added, start + added));
        Change {
            cursor: Some((start, end)),
            restart: false,
        }
    }
}

/// Wipe typed text from events egui is done with.
pub fn wipe_text(events: &mut [Event]) {
    for event in events {
        if let Event::Text(text) | Event::Paste(text) = event {
            text.zeroize();
        }
    }
}

/// The field's cursor or selection, in characters (sorted).
pub fn cursor(ctx: &Context, field: Id) -> Option<(usize, usize)> {
    let range = TextEditState::load(ctx, field)?.cursor.char_range()?;
    Some(sorted(range))
}

fn sorted(range: CCursorRange) -> (usize, usize) {
    let [a, b] = range.sorted_cursors();
    (a.index.0, b.index.0)
}

pub fn set_cursor(ctx: &Context, field: Id, (a, b): (usize, usize)) {
    if let Some(mut state) = TextEditState::load(ctx, field) {
        let range = CCursorRange::two(CCursor::new(a), CCursor::new(b));
        state.cursor.set_char_range(Some(range));
        state.store(ctx, field);
    }
}

/// Drops winit's "Unknown android_activity input event {event:?}", which holds the
/// keyboard's text, before it is formatted or written. Other warnings stay.
pub struct NoTypedText;

const TYPED_TEXT_EVENT: &str = "Unknown android_activity input event";

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for NoTypedText {
    fn event_enabled(
        &self,
        event: &tracing::Event<'_>,
        _: tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        if !event
            .metadata()
            .target()
            .starts_with("winit::platform_impl::android")
        {
            return true;
        }
        let mut start = MessageStart::default();
        event.record(&mut start);
        start.0 != TYPED_TEXT_EVENT
    }
}

/// The first words of an event's message; formatting stops there, so the rest
/// (here: the typed text) is never written out, not even in memory.
#[derive(Default)]
struct MessageStart(String);

impl tracing::field::Visit for MessageStart {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self, "{value:?}");
        }
    }
}

impl fmt::Write for MessageStart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut end = s
            .len()
            .min(TYPED_TEXT_EVENT.len().saturating_sub(self.0.len()));
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push_str(&s[..end]);
        match end == s.len() {
            true => Ok(()),
            false => Err(fmt::Error),
        }
    }
}

/// What the keyboard is set up for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Normal,
    /// Recovery words: shown, but no suggestions and no learning.
    Private,
    Password,
}

/// A step of the Java-thread job at the end of a frame.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// Overwrite the buffer a secret field used, then empty it.
    Wipe,
    /// Set the keyboard type (and the length cap it clears).
    Settings(Kind),
    /// Note the action key count: later presses belong to the new field.
    CountActions,
}

/// Focus and keyboard type across frames. Both are decided at the end of a frame,
/// so leaving a secret field and setting up the next field's keyboard are one job
/// in that order: a keyboard that reconnects in between can't read the secret
/// with learning on.
#[derive(Default)]
pub struct Focus {
    pub focused: Option<Id>,
    kind: Option<Kind>,
    /// A secret field had focus since the buffer was last wiped.
    secret: bool,
}

impl Focus {
    /// `focused` at the end of a frame, and the kind of the field there if it
    /// wants text. The steps to run, in order, and whether focus moved.
    pub fn frame_end(&mut self, focused: Option<Id>, kind: Option<Kind>) -> (Vec<Step>, bool) {
        let mut steps = Vec::new();
        let moved = focused != self.focused;
        if moved {
            self.focused = focused;
            if std::mem::take(&mut self.secret) {
                steps.push(Step::Wipe);
            }
            steps.push(Step::CountActions);
        }
        if let Some(kind) = kind {
            self.secret |= kind != Kind::Normal;
            if self.kind != Some(kind) {
                self.kind = Some(kind);
                steps.push(Step::Settings(kind));
            }
        }
        (steps, moved)
    }
}

#[cfg(target_os = "android")]
mod activity {
    use std::{
        ffi::{c_char, c_void},
        path::PathBuf,
        slice,
        sync::{
            LazyLock, Mutex, MutexGuard, OnceLock,
            atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
        },
        time::{Duration, Instant},
    };

    use anyhow::{Context as _, Result};
    use eframe::{
        UserEvent,
        egui::{self, IMEPurpose, Id, SafeAreaInsets, epaint::MarginF32, text::CCursorRange},
    };
    use jni::{JValue, JavaVM, jni_sig, jni_str, objects::JObject};
    use winit::{
        application::ApplicationHandler,
        event::{DeviceEvent, DeviceId, StartCause, WindowEvent},
        event_loop::{ActiveEventLoop, EventLoop},
        platform::android::{
            EventLoopBuilderExtAndroid as _,
            activity::{
                AndroidApp,
                input::{ImeOptions, InputType},
            },
        },
        window::WindowId,
    };

    use super::{
        Bridge, Buffer, Focus, Kind, MAX_TYPED, NATIVE_BUFFER, Step, copied, cursor, forget_shared,
        has_shared, set_cursor, start, whole_chars, wipe_text,
    };

    /// The keyboard's action key (Done): Enter for egui.
    fn enter(events: &mut Vec<egui::Event>) {
        for pressed in [true, false] {
            events.push(egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            });
        }
    }

    /// One activity per process: `android_main` ends the process after it.
    static APP: OnceLock<AndroidApp> = OnceLock::new();
    /// The settings folder, kept for when the activity is gone.
    static DATA: OnceLock<PathBuf> = OnceLock::new();
    static KEYBOARD: LazyLock<Mutex<Keyboard>> = LazyLock::new(Mutex::default);
    /// The buffer as last read on the Java thread.
    static READ: Mutex<Option<Read>> = Mutex::new(None);
    /// The action key count when focus last moved, for start-over number `.0`
    /// of focus: later presses belong to the newly focused field.
    static FOCUS_ACTIONS: Mutex<Option<(u64, i32)>> = Mutex::new(None);
    /// System bars, display cutout and keyboard, in pixels: left, top, right, bottom.
    static INSETS: Mutex<[i32; 4]> = Mutex::new([0; 4]);
    /// The window or the keyboard changed: follow the insets for a moment.
    static INSETS_STALE: AtomicBool = AtomicBool::new(true);
    /// Back presses MainActivity passed on, not yet handled (`crate::back`).
    static BACK_PRESSES: AtomicU32 = AtomicU32::new(0);
    /// Whether MainActivity takes Back: 0 no, 1 yes, 2 not told yet.
    static BACK_ENABLED: AtomicU8 = AtomicU8::new(2);
    /// For a Back press to wake the app.
    static CTX: OnceLock<egui::Context> = OnceLock::new();
    /// `WindowInsetsCompat.Type`: status bars 1, navigation bars 2, caption bar 4,
    /// keyboard 8, display cutout 128.
    const COVERED: i32 = 1 | 2 | 4 | 8 | 128;
    /// `EditorInfo.IME_ACTION_DONE`, in `imeOptions`: the keyboard shows Done, which
    /// is Enter.
    const ACTION_DONE: i32 = 6;
    /// How long insets are read every frame after a change (the keyboard slides).
    const INSETS_SETTLE: Duration = Duration::from_millis(600);
    /// While the keyboard is up it can be closed with Back, which tells the app nothing.
    const KEYBOARD_CHECK: Duration = Duration::from_millis(500);
    /// How often a frame reads the keyboard's buffer while a field is focused:
    /// nothing tells the app that it changed.
    const TYPING_POLL: Duration = Duration::from_millis(40);
    /// A start-over the Java thread hasn't confirmed by then is sent again.
    const RESTART_RETRY: Duration = Duration::from_millis(300);
    /// Frames a newly focused field gets to share its text before the buffer
    /// starts empty.
    const SHARE_WAIT: u8 = 3;
    /// How long a destroyed activity waits for messages still being sent. A new
    /// activity's window waits too, and Android calls it not responding after 5 s.
    const SEND_GRACE: Duration = Duration::from_secs(4);

    /// The buffer and the count of action key presses, read together.
    #[derive(Clone)]
    struct Read {
        generation: u64,
        buffer: Buffer,
        actions: i32,
    }

    struct Keyboard {
        bridge: Bridge,
        focus: Focus,
        /// Start the buffer over (with the field's text) once the field has a cursor;
        /// `on_focus` for the first start after focus moved.
        start: bool,
        on_focus: bool,
        waited: u8,
        /// Start-overs sent; the field waits until the Java thread confirmed the
        /// latest (`confirmed` false, sent at `sent`).
        generation: u64,
        confirmed: bool,
        sent: Instant,
        /// Focus moves so far, and the action key presses turned into Enter for the
        /// focused field (unknown until the Java thread said how many there were).
        focus_moves: u64,
        actions: Option<i32>,
        ime: bool,
        insets_until: Instant,
        insets_next: Instant,
        /// Insets at the last frame start, to bring the field back into view.
        insets: [i32; 4],
    }

    impl Default for Keyboard {
        fn default() -> Self {
            Self {
                bridge: Bridge::default(),
                focus: Focus::default(),
                start: false,
                on_focus: false,
                waited: 0,
                generation: 0,
                confirmed: true,
                sent: Instant::now(),
                focus_moves: 0,
                actions: None,
                ime: false,
                insets_until: Instant::now(),
                insets_next: Instant::now(),
                insets: [0; 4],
            }
        }
    }

    impl Keyboard {
        /// Focus moved: forget the last field's text at once, keeping what the new
        /// field shared, and start its buffer over (after the Java-thread job of
        /// this frame end, which the start-over follows in the same queue).
        fn forget(&mut self, ctx: &egui::Context, focused: Option<Id>) {
            self.bridge = Bridge::default();
            *lock(&READ) = None;
            forget_shared(ctx, focused);
            self.start = true;
            self.on_focus = true;
            self.waited = 0;
            self.confirmed = true;
            self.focus_moves += 1;
            self.actions = None;
        }

        /// Put the bridge's buffer into the keyboard's, as start-over `generation`.
        fn send(&mut self, app: &AndroidApp, ctx: &egui::Context, again: bool) {
            if !again {
                self.generation += 1;
            }
            self.confirmed = false;
            self.sent = Instant::now();
            let (text, selection) = self.bridge.buffer().to_java();
            let (generation, ctx) = (self.generation, ctx.clone());
            on_java_thread(app, move |input, actions| {
                // SAFETY: `input` is live on this thread; the text ends in NUL.
                unsafe { set_state(input, &text, selection) };
                read_back(input, generation, actions, &ctx);
            });
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run eframe until the activity is destroyed. winit 0.30 ignores `onDestroy`,
    /// which then waits for `android_main` to return.
    pub fn run(options: eframe::NativeOptions, creator: eframe::AppCreator<'_>) -> Result<()> {
        let app = options.android_app.clone().context("no Android activity")?;
        if let Some(data) = app.internal_data_path() {
            let _ = DATA.set(data);
        }
        // The length cap, before any field takes focus.
        configure_keyboard(&app, Kind::Normal);
        let event_loop = EventLoop::<UserEvent>::with_user_event()
            .with_android_app(app.clone())
            .build()?;
        let eframe = eframe::create_native("RustTalk", options, creator, &event_loop);
        event_loop.run_app(&mut UntilDestroyed { eframe, app })?;
        Ok(())
    }

    struct UntilDestroyed<'a> {
        eframe: eframe::EframeWinitApplication<'a>,
        app: AndroidApp,
    }

    impl ApplicationHandler<UserEvent> for UntilDestroyed<'_> {
        fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
            self.eframe.new_events(event_loop, cause);
        }

        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            INSETS_STALE.store(true, Ordering::Relaxed);
            self.eframe.resumed(event_loop);
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
            self.eframe.user_event(event_loop, event);
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
            if matches!(
                event,
                WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }
            ) {
                INSETS_STALE.store(true, Ordering::Relaxed);
            }
            self.eframe.window_event(event_loop, id, event);
        }

        fn device_event(&mut self, event_loop: &ActiveEventLoop, id: DeviceId, event: DeviceEvent) {
            self.eframe.device_event(event_loop, id, event);
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            self.eframe.about_to_wait(event_loop);
            // Every loop pass ends here, also the one that handled `onDestroy`.
            if destroyed(&self.app) {
                tracing::info!("Android activity destroyed");
                // The process ends next; the app's tasks don't get to finish. What
                // is still on its way gets a notice at the next start.
                if crate::net::wait_for_sends(SEND_GRACE) > 0
                    && let Some(data) = DATA.get()
                {
                    crate::net::note_unsent(data);
                }
                event_loop.exit();
            }
        }

        fn suspended(&mut self, event_loop: &ActiveEventLoop) {
            self.eframe.suspended(event_loop);
        }

        fn exiting(&mut self, event_loop: &ActiveEventLoop) {
            self.eframe.exiting(event_loop);
        }

        fn memory_warning(&mut self, event_loop: &ActiveEventLoop) {
            self.eframe.memory_warning(event_loop);
        }
    }

    /// android-activity forgets the activity once it handled `onDestroy`; the app
    /// folder is always there before (also when a new activity ends this one).
    fn destroyed(app: &AndroidApp) -> bool {
        app.internal_data_path().is_none()
    }

    #[unsafe(no_mangle)]
    extern "system" fn Java_io_github_rusttalkcrew_rusttalk_MainActivity_initializeVoice(
        env: livekit_jni::JNIEnv,
        _: livekit_jni::objects::JClass,
        context: livekit_jni::objects::JObject,
    ) -> livekit_jni::sys::jboolean {
        env.get_java_vm()
            .is_ok_and(|vm| livekit::webrtc::android::initialize_android_context(&vm, &context))
            .into()
    }

    pub fn voice_permission(request: bool) -> i32 {
        let Some(app) = APP.get() else { return -2 };
        java(app, |env, activity| {
            env.call_method(
                activity,
                jni_str!("voicePermission"),
                jni_sig!("(Z)I"),
                &[JValue::Bool(request)],
            )?
            .i()
        })
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "checking microphone permission failed");
            -2
        })
    }

    pub fn communication_mode(on: bool) {
        if let Some(app) = APP.get()
            && let Err(error) = java(app, |env, activity| {
                env.call_method(
                    activity,
                    jni_str!("communicationMode"),
                    jni_sig!("(Z)V"),
                    &[JValue::Bool(on)],
                )?
                .v()
            })
        {
            tracing::warn!(%error, "setting call audio mode failed");
        }
    }

    /// MainActivity's Back callback and the Back key (which GameActivity would give
    /// winit) both end here, on the Java main thread.
    #[unsafe(no_mangle)]
    extern "system" fn Java_io_github_rusttalkcrew_rusttalk_MainActivity_backPressed(
        _: *mut c_void,
        _: *mut c_void,
    ) {
        BACK_PRESSES.fetch_add(1, Ordering::Relaxed);
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
    }

    /// Whether the app takes Back: while something is open to close. At the root
    /// Android handles it, with the predictive back animation on Android 13 and newer.
    pub fn back_enabled(on: bool) {
        if BACK_ENABLED.swap(u8::from(on), Ordering::Relaxed) == u8::from(on) {
            return;
        }
        let Some(app) = APP.get() else { return };
        if let Err(error) = java(app, |env, activity| {
            env.call_method(
                activity,
                jni_str!("backEnabled"),
                jni_sig!("(Z)V"),
                &[JValue::Bool(on)],
            )?
            .v()
        }) {
            tracing::warn!(%error, "setting Back handling failed");
        }
    }

    /// Back at the root: to the background like other apps, not finished.
    pub fn to_background() {
        if let Some(app) = APP.get()
            && let Err(error) = java(app, |env, activity| {
                env.call_method(activity, jni_str!("toBackground"), jni_sig!("()V"), &[])?
                    .v()
            })
        {
            tracing::warn!(%error, "moving to the background failed");
        }
    }

    pub fn init(app: AndroidApp) {
        let _ = APP.set(app);
    }

    /// The activity, for Java calls elsewhere (`phone_clipboard`).
    pub fn app() -> Option<&'static AndroidApp> {
        APP.get()
    }

    pub fn java<T>(
        app: &AndroidApp,
        f: impl FnOnce(&mut jni::Env, &JObject) -> jni::errors::Result<T>,
    ) -> jni::errors::Result<T> {
        // SAFETY: android-activity holds the VM for as long as `app` lives.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        // Scopes local references to this call and turns Java exceptions into errors.
        vm.attach_current_thread(|env| {
            let raw: jni::sys::jobject = app.activity_as_ptr().cast();
            // SAFETY: android-activity keeps this global reference while `app` lives;
            // the cast doesn't own it.
            let global = unsafe { env.as_cast_raw::<JObject>(&raw)? };
            let activity = env.new_local_ref(&*global)?;
            f(env, &activity)
        })
    }

    #[repr(C)]
    struct Span {
        start: i32,
        end: i32,
    }

    /// `GameTextInputState`.
    #[repr(C)]
    struct State {
        text: *const c_char,
        length: i32,
        selection: Span,
        composing: Span,
    }

    // GameTextInput's C API, built into this library with android-activity.
    unsafe extern "C" {
        fn GameActivity_getTextInput(activity: *const c_void) -> *mut c_void;
        fn GameTextInput_getState(
            input: *mut c_void,
            callback: unsafe extern "C" fn(*mut c_void, *const State),
            context: *mut c_void,
        );
        fn GameTextInput_setState(input: *mut c_void, state: *const State);
    }

    /// Set the buffer; `text` is modified UTF-8 ending in NUL.
    ///
    /// SAFETY: `input` is GameTextInput, used on the Java thread.
    unsafe fn set_state(input: *mut c_void, text: &[u8], selection: (i32, i32)) {
        let state = State {
            text: text.as_ptr().cast(),
            length: i32::try_from(text.len() - 1).unwrap_or(0),
            selection: Span {
                start: selection.0,
                end: selection.1,
            },
            composing: Span { start: -1, end: -1 },
        };
        unsafe { GameTextInput_setState(input, &state) };
    }

    /// SAFETY: `input` is GameTextInput, used on the Java thread.
    unsafe fn read_state(input: *mut c_void) -> Option<Buffer> {
        unsafe extern "C" fn copy(context: *mut c_void, state: *const State) {
            // SAFETY: `context` is `out` below; the state is valid during this call.
            let (out, state) = unsafe { (&mut *context.cast::<Option<Buffer>>(), state.as_ref()) };
            let Some(state) = state else { return };
            // Only what GameTextInput really copied; the text is null before the
            // first state is set.
            let length = copied(state.length);
            let bytes = match state.text.is_null() || length == 0 {
                true => &[][..],
                false => {
                    whole_chars(unsafe { slice::from_raw_parts(state.text.cast::<u8>(), length) })
                }
            };
            let selection = (state.selection.start, state.selection.end);
            *out = Some(Buffer::from_java(bytes, selection));
        }
        let mut out = None;
        unsafe { GameTextInput_getState(input, copy, (&raw mut out).cast()) };
        out
    }

    /// Read the buffer for the frame that sent start-over `generation`.
    fn read_back(input: *mut c_void, generation: u64, actions: i32, ctx: &egui::Context) {
        // SAFETY: called on the Java thread with a live `input`.
        let Some(buffer) = (unsafe { read_state(input) }) else {
            return;
        };
        let mut read = lock(&READ);
        let changed = read.as_ref().is_none_or(|old| {
            old.generation != generation || old.actions != actions || old.buffer != buffer
        });
        if changed {
            *read = Some(Read {
                generation,
                buffer,
                actions,
            });
            ctx.request_repaint();
        }
    }

    /// This activity's GameTextInput and the action key count; on the Java thread.
    fn text_input(app: &AndroidApp) -> Option<(*mut c_void, i32)> {
        let found = java(app, |env, activity| {
            // This activity's GameActivity, unless a newer one took over.
            // (`getGameActivityNativeHandle` returns 0 for the current one.)
            let class = env.get_object_class(activity)?;
            let owner = env
                .get_static_field(
                    &class,
                    jni_str!("nativeHandleOwner"),
                    jni_sig!("Lcom/google/androidgamesdk/GameActivity;"),
                )?
                .l()?;
            if !env.is_same_object(&owner, activity)? {
                return Ok((0, 0));
            }
            let handle = env
                .get_static_field(&class, jni_str!("nativeHandle"), jni_sig!("J"))?
                .j()?;
            // MainActivity counts the Done key, which winit drops.
            let actions = env
                .get_static_field(&class, jni_str!("editorActions"), jni_sig!("I"))?
                .i()?;
            Ok((handle, actions))
        });
        let (handle, actions) = match found {
            Ok((0, _)) => return None,
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(%error, "find the keyboard's text");
                return None;
            }
        };
        // SAFETY: the live GameActivity; it is only freed on this thread.
        let input = unsafe { GameActivity_getTextInput(handle as *const c_void) };
        (!input.is_null()).then_some((input, actions))
    }

    /// Run `f` with GameTextInput and the action key count on the Java thread,
    /// where the keyboard edits the buffer.
    fn on_java_thread(app: &AndroidApp, f: impl FnOnce(*mut c_void, i32) + Send + 'static) {
        let main = app.clone();
        app.run_on_java_main_thread(Box::new(move || {
            if let Some((input, actions)) = text_input(&main) {
                f(input, actions);
            }
        }));
    }

    /// The frame end's steps as one Java-thread job, in their order.
    fn run_steps(app: &AndroidApp, steps: Vec<Step>, moves: u64) {
        let main = app.clone();
        app.run_on_java_main_thread(Box::new(move || {
            let found = text_input(&main);
            for step in steps {
                match (step, found) {
                    (Step::Wipe, Some((input, _))) => {
                        let mut filler = vec![b'x'; NATIVE_BUFFER - 1];
                        filler.push(0);
                        // SAFETY: `input` is live on this thread; the texts end in NUL.
                        unsafe { set_state(input, &filler, (0, 0)) };
                        unsafe { set_state(input, b"\0", (0, 0)) };
                    }
                    (Step::Settings(kind), _) => {
                        if let Err(error) = apply_settings(&main, kind) {
                            tracing::warn!(%error, "set up the keyboard");
                        }
                    }
                    (Step::CountActions, Some((_, actions))) => {
                        *lock(&FOCUS_ACTIONS) = Some((moves, actions));
                    }
                    _ => {}
                }
            }
        }));
    }

    /// Set the keyboard type and the length cap from the start.
    fn configure_keyboard(app: &AndroidApp, kind: Kind) {
        let main = app.clone();
        app.run_on_java_main_thread(Box::new(move || {
            if let Err(error) = apply_settings(&main, kind) {
                tracing::warn!(%error, "set up the keyboard");
            }
        }));
    }

    /// The keyboard type for the field, and the length cap; on the Java thread,
    /// because setting the type clears the cap.
    fn apply_settings(app: &AndroidApp, kind: Kind) -> jni::errors::Result<()> {
        // Multi-line keeps newlines in pasted text; Done is the action key (Enter).
        let mut input = InputType::TYPE_CLASS_TEXT | InputType::TYPE_TEXT_FLAG_MULTI_LINE;
        let mut options = ImeOptions::IMG_FLAG_NO_EXTRACT_UI | ImeOptions::IME_FLAG_NO_FULLSCREEN;
        if kind != Kind::Normal {
            // No suggestions, and the keyboard must not learn it.
            input |= match kind {
                Kind::Password => InputType::TYPE_TEXT_VARIATION_PASSWORD,
                _ => InputType::TYPE_TEXT_VARIATION_VISIBLE_PASSWORD,
            } | InputType::TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            options |= ImeOptions::IME_FLAG_NO_PERSONALIZED_LEARNING;
        }
        let (input, options) = (input.bits() as i32, options.bits() as i32 | ACTION_DONE);
        java(app, |env, activity| {
            env.call_method(
                activity,
                jni_str!("setImeEditorInfoFields"),
                jni_sig!("(III)V"),
                &[
                    JValue::Int(input),
                    JValue::Int(ACTION_DONE),
                    JValue::Int(options),
                ],
            )?;
            let view = env
                .get_field(
                    activity,
                    jni_str!("mSurfaceView"),
                    jni_sig!("Lcom/google/androidgamesdk/GameActivity$InputEnabledSurfaceView;"),
                )?
                .l()?;
            let connection = env
                .get_field(
                    &view,
                    jni_str!("mInputConnection"),
                    jni_sig!("Lcom/google/androidgamesdk/gametextinput/InputConnection;"),
                )?
                .l()?;
            let editable = env
                .call_method(
                    &connection,
                    jni_str!("getEditable"),
                    jni_sig!("()Landroid/text/Editable;"),
                    &[],
                )?
                .l()?;
            // GameTextInput copies at most 64 KiB but reports the full length,
            // which winit's reader trusts: keep the text short enough to fit.
            let filter = env.new_object(
                jni_str!("android/text/InputFilter$LengthFilter"),
                jni_sig!("(I)V"),
                &[JValue::Int(MAX_TYPED)],
            )?;
            let filters = env.new_object_array(1, jni_str!("android/text/InputFilter"), &filter)?;
            env.call_method(
                &editable,
                jni_str!("setFilters"),
                jni_sig!("([Landroid/text/InputFilter;)V"),
                &[JValue::Object(&filters)],
            )?;
            Ok(())
        })
    }

    /// Ask the activity for its insets on its own thread (views aren't thread
    /// safe); a change paints a new frame and keeps following them.
    fn query_insets(app: &AndroidApp, ctx: &egui::Context) {
        let (main, ctx) = (app.clone(), ctx.clone());
        app.run_on_java_main_thread(Box::new(move || {
            let insets = java(&main, |env, activity| {
                let insets = env
                    .call_method(
                        activity,
                        jni_str!("getWindowInsets"),
                        jni_sig!("(I)Landroidx/core/graphics/Insets;"),
                        &[JValue::Int(COVERED)],
                    )?
                    .l()?;
                if insets.is_null() {
                    return Ok([0; 4]);
                }
                let mut side =
                    |name: &jni::strings::JNIStr| env.get_field(&insets, name, jni_sig!("I"))?.i();
                Ok([
                    side(jni_str!("left"))?,
                    side(jni_str!("top"))?,
                    side(jni_str!("right"))?,
                    side(jni_str!("bottom"))?,
                ])
            });
            match insets {
                Ok(insets) => {
                    let mut current = lock(&INSETS);
                    if *current != insets {
                        *current = insets;
                        INSETS_STALE.store(true, Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                }
                Err(error) => tracing::warn!(%error, "read window insets"),
            }
        }));
    }

    /// Before the frame: typed text, and the space the system leaves for the app.
    pub fn input(ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let Some(app) = APP.get() else { return };
        let _ = CTX.set(ctx.clone());
        // One press a frame: each goes to what the frame before showed on top.
        if BACK_PRESSES
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
        {
            crate::back::press(&mut raw_input.events);
            if BACK_PRESSES.load(Ordering::Relaxed) > 0 {
                ctx.request_repaint();
            }
        }
        let mut keyboard = lock(&KEYBOARD);
        let now = Instant::now();
        if INSETS_STALE.swap(false, Ordering::Relaxed) {
            keyboard.insets_until = now + INSETS_SETTLE;
        }
        if now < keyboard.insets_until || (keyboard.ime && now >= keyboard.insets_next) {
            keyboard.insets_next = now + KEYBOARD_CHECK;
            query_insets(app, ctx);
        }
        let [left, top, right, bottom] = lock(&INSETS).map(|px| px as f32 / ctx.pixels_per_point());
        raw_input.safe_area_insets = Some(SafeAreaInsets(MarginF32 {
            left,
            right,
            top,
            bottom,
        }));

        // Focus moves are handled at the frame end (`frame_end`).
        let focused = ctx.memory(|memory| memory.focused());
        let Some(field) = focused
            .filter(|_| ctx.egui_wants_keyboard_input())
            .filter(|_| focused == keyboard.focus.focused)
        else {
            return;
        };
        let Some(cursor) = cursor(ctx, field) else {
            return;
        };
        if keyboard.start {
            // A field shares itself from its first frame with focus.
            if !has_shared(ctx, field) && keyboard.waited < SHARE_WAIT {
                keyboard.waited += 1;
                return;
            }
            let on_focus = keyboard.on_focus;
            let at = start(ctx, &mut keyboard.bridge, field, cursor, on_focus);
            if at != cursor {
                set_cursor(ctx, field, at);
            }
            keyboard.start = false;
            keyboard.on_focus = false;
            keyboard.send(app, ctx, false);
            return;
        }
        if keyboard.actions.is_none()
            && let Some((moves, actions)) = *lock(&FOCUS_ACTIONS)
            && moves == keyboard.focus_moves
        {
            keyboard.actions = Some(actions);
        }
        let read = lock(&READ)
            .clone()
            .filter(|read| read.generation == keyboard.generation);
        if !keyboard.confirmed {
            // Only the exact buffer that was sent confirms the start-over.
            match read {
                Some(read) if read.buffer == *keyboard.bridge.buffer() => {
                    keyboard.confirmed = true;
                }
                _ => {
                    if keyboard.sent.elapsed() >= RESTART_RETRY {
                        keyboard.send(app, ctx, true);
                    }
                    return;
                }
            }
        } else if let Some(read) = read {
            let change = keyboard
                .bridge
                .update(cursor, &read.buffer, &mut raw_input.events);
            if let Some(range) = change.cursor {
                set_cursor(ctx, field, range);
            }
            if change.restart {
                keyboard.bridge = Bridge::default();
                keyboard.start = true;
                return;
            }
            // Each press once, for the field focused when it came.
            if let Some(seen) = keyboard.actions
                && read.actions > seen
            {
                for _ in seen..read.actions {
                    enter(&mut raw_input.events);
                }
                keyboard.actions = Some(read.actions);
            }
        }
        let (generation, ctx) = (keyboard.generation, ctx.clone());
        on_java_thread(app, move |input, actions| {
            read_back(input, generation, actions, &ctx)
        });
    }

    /// Draw the app in the part of the screen the system bars, notch and keyboard
    /// leave free; eframe's root `Ui` covers the whole screen.
    pub fn frame(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
        let ctx = ui.ctx().clone();
        keep_field_visible(&ctx);
        ui.painter()
            .rect_filled(ctx.viewport_rect(), 0.0, ui.visuals().panel_fill);
        let free = ctx.content_rect();
        ui.scope_builder(egui::UiBuilder::new().max_rect(free), |ui| {
            ui.set_clip_rect(free);
            add(ui);
        });
        frame_end(&ctx);
    }

    /// Bring the focused field back into view when the keyboard covers it.
    fn keep_field_visible(ctx: &egui::Context) {
        let mut keyboard = lock(&KEYBOARD);
        let insets = *lock(&INSETS);
        if keyboard.insets != insets {
            keyboard.insets = insets;
            if let Some(field) = keyboard.focus.focused.and_then(|id| ctx.read_response(id)) {
                field.scroll_to_me(None);
            }
        }
    }

    /// At the end of the frame: wipe the typed text egui got, and tell the keyboard
    /// what the focused field is for.
    fn frame_end(ctx: &egui::Context) {
        ctx.input_mut(|input| {
            wipe_text(&mut input.events);
            wipe_text(&mut input.raw.events);
        });
        let Some(app) = APP.get() else { return };
        let mut keyboard = lock(&KEYBOARD);
        let ime = ctx.output(|output| output.ime);
        if ime.is_some() != keyboard.ime {
            keyboard.ime = ime.is_some();
            INSETS_STALE.store(true, Ordering::Relaxed);
            if ime.is_none() {
                // winit hides only a keyboard shown implicitly, and Android counts
                // some of ours as shown on purpose.
                app.hide_soft_input(false);
            }
        }
        if Instant::now() < keyboard.insets_until || ime.is_some() {
            ctx.request_repaint_after(TYPING_POLL);
        }
        // Focus and the keyboard type, decided together: leaving a secret field
        // wipes the buffer before the next field's keyboard type is set.
        let focused = ctx.memory(|memory| memory.focused());
        let private = focused.is_some_and(|field| crate::ui::is_private(ctx, field));
        let kind = ime.map(|ime| match ime.purpose {
            IMEPurpose::Password => Kind::Password,
            _ if private => Kind::Private,
            _ => Kind::Normal,
        });
        let (steps, moved) = keyboard.focus.frame_end(focused, kind);
        if moved {
            keyboard.forget(ctx, focused);
        }
        if !steps.is_empty() {
            run_steps(app, steps, keyboard.focus_moves);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{RawInput, TextEdit};
    use tracing_subscriber::layer::SubscriberExt as _;

    const FIELD: &str = "field";

    /// A focused egui field and the bridge in front of it.
    struct Field {
        ctx: Context,
        text: String,
        multiline: bool,
        bridge: Bridge,
    }

    impl Field {
        /// The buffer starts with the field's text around the cursor, as on a phone.
        fn new(text: &str, multiline: bool, cursor: (usize, usize)) -> Self {
            let mut field = Self {
                ctx: Context::default(),
                text: text.to_owned(),
                multiline,
                bridge: Bridge::default(),
            };
            field
                .ctx
                .memory_mut(|memory| memory.request_focus(Id::unique(FIELD)));
            field.frame(Vec::new());
            set_cursor(&field.ctx, Id::unique(FIELD), cursor);
            field.frame(Vec::new());
            let seed = Seed::around(&field.text, field.cursor());
            field.bridge.start(field.cursor(), seed);
            field
        }

        fn frame(&mut self, events: Vec<Event>) {
            let input = RawInput {
                events,
                ..RawInput::default()
            };
            let (text, multiline) = (&mut self.text, self.multiline);
            let mut output = self.ctx.run_ui(input, |ui| {
                let edit = match multiline {
                    true => TextEdit::multiline(text),
                    false => TextEdit::singleline(text),
                };
                ui.add(edit.id(Id::unique(FIELD)));
            });
            output.textures_delta.clear();
        }

        fn cursor(&self) -> (usize, usize) {
            cursor(&self.ctx, Id::unique(FIELD)).unwrap()
        }

        /// The keyboard's buffer is now `text`, selection in characters.
        fn keyboard(&mut self, text: &str, selection: (usize, usize)) -> Change {
            let buffer = Buffer {
                text: Zeroizing::new(text.to_owned()),
                selection,
            };
            let mut events = Vec::new();
            let change = self.bridge.update(self.cursor(), &buffer, &mut events);
            if let Some(range) = change.cursor {
                set_cursor(&self.ctx, Id::unique(FIELD), range);
            }
            self.frame(events);
            change
        }
    }

    #[test]
    fn typing_into_an_empty_field() {
        let mut field = Field::new("", false, (0, 0));
        field.keyboard("h", (1, 1));
        field.keyboard("hi", (2, 2));
        assert_eq!(field.text, "hi");
        assert_eq!(field.cursor(), (2, 2));
    }

    #[test]
    fn the_buffer_starts_with_the_text_around_the_cursor() {
        let mut field = Field::new("hello world", false, (3, 3));
        assert_eq!(&*field.bridge.buffer().text, "hello world");
        assert_eq!(field.bridge.buffer().selection, (3, 3));
        // Backspace right after tapping into the word. Which "l" went is
        // ambiguous; the keyboard's cursor follows next frame.
        field.keyboard("helo world", (2, 2));
        assert_eq!(field.text, "helo world");
        field.keyboard("helo world", (2, 2));
        assert_eq!(field.cursor(), (2, 2));
    }

    #[test]
    fn composing_in_the_middle_keeps_the_text_after_it() {
        // "a|b": Japanese input composes にほん between them, then converts it.
        let mut field = Field::new("ab", false, (1, 1));
        field.keyboard("aにb", (2, 2));
        field.keyboard("aにほんb", (4, 4));
        field.keyboard("a日本b", (3, 3));
        assert_eq!(field.text, "a日本b");
        assert_eq!(field.cursor(), (3, 3));
    }

    #[test]
    fn an_emoji_before_the_change_counts_as_one_character() {
        let mut field = Field::new("😀ab", false, (2, 2));
        field.keyboard("😀axb", (3, 3));
        field.keyboard("😀axyb", (4, 4));
        assert_eq!(field.text, "😀axyb");
        assert_eq!(field.cursor(), (4, 4));
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut field = Field::new("hello world", false, (6, 11));
        field.keyboard("hello there", (11, 11));
        assert_eq!(field.text, "hello there");
        assert_eq!(field.cursor(), (11, 11));
    }

    #[test]
    fn an_empty_buffer_stands_for_the_selection() {
        let mut field = Field::new("hello world", false, (6, 11));
        field.bridge.start(field.cursor(), Seed::default());
        field.keyboard("there", (5, 5));
        assert_eq!(field.text, "hello there");
    }

    #[test]
    fn autocorrect_replaces_the_word() {
        let mut field = Field::new("", false, (0, 0));
        field.keyboard("lonly", (5, 5));
        field.keyboard("lonely ", (7, 7));
        assert_eq!(field.text, "lonely ");
    }

    #[test]
    fn deleting_across_the_composed_word() {
        let mut field = Field::new("xy", false, (1, 1));
        field.keyboard("xabcy", (4, 4));
        // The keyboard selects "bc" (its composed part) and deletes it.
        field.keyboard("xabcy", (2, 4));
        assert_eq!(field.cursor(), (2, 4));
        field.keyboard("xay", (2, 2));
        assert_eq!(field.text, "xay");
        assert_eq!(field.cursor(), (2, 2));
    }

    #[test]
    fn newlines_stay_text_even_alone() {
        let mut field = Field::new("", true, (0, 0));
        field.keyboard("a\nb", (3, 3));
        field.keyboard("a\nb\n", (4, 4));
        assert_eq!(field.text, "a\nb\n");
        let mut events = Vec::new();
        let buffer = Buffer {
            text: Zeroizing::new("a\nb\n\n".to_owned()),
            selection: (5, 5),
        };
        field.bridge.update(field.cursor(), &buffer, &mut events);
        assert!(matches!(&events[..], [Event::Paste(text)] if text == "\n"));
    }

    #[test]
    fn moving_the_cursor_in_the_field_starts_over() {
        let mut field = Field::new("abc", false, (3, 3));
        field.keyboard("abcd", (4, 4));
        set_cursor(&field.ctx, Id::unique(FIELD), (0, 0));
        assert!(field.keyboard("abcde", (5, 5)).restart);
        assert_eq!(field.text, "abcd");
    }

    #[test]
    fn a_paste_over_64_kib() {
        let long = "y".repeat(70_000);
        // Whatever the length says, only what GameTextInput copied is read.
        assert_eq!(
            copied(i32::try_from(long.len()).unwrap()),
            NATIVE_BUFFER - 1
        );
        assert_eq!(copied(-1), 0);
        let buffer = Buffer::from_java(&long.as_bytes()[..copied(70_000)], (-1, -1));
        assert_eq!(buffer.text.len(), NATIVE_BUFFER - 1);
        // The keyboard keeps less than that, and the field takes all of it.
        assert!(MAX_TYPED as usize * 3 < NATIVE_BUFFER);
        let paste = "z".repeat(MAX_TYPED as usize);
        let mut field = Field::new("", true, (0, 0));
        field.keyboard(&paste, (paste.len(), paste.len()));
        assert_eq!(field.text, paste);
    }

    #[test]
    fn java_text_and_selection() {
        // "a", NUL as C0 80, 😀 as two 3-byte surrogates, then "b".
        let bytes = [b'a', 0xc0, 0x80, 0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80, b'b'];
        // UTF-16: a=0, NUL=1, 😀=2..4, b=4.
        let buffer = Buffer::from_java(&bytes, (4, 2));
        assert_eq!(&*buffer.text, "a\0😀b");
        assert_eq!(buffer.selection, (2, 3));
        let (back, selection) = buffer.to_java();
        assert_eq!(&back[..], &[&bytes[..], &[0]].concat()[..]);
        assert_eq!(selection, (2, 4));
        assert_eq!(Buffer::from_java(b"ab", (-1, -1)).selection, (2, 2));
        assert_eq!(&*Buffer::from_java(&[0xff, b'a'], (0, 0)).text, "\u{fffd}a");
    }

    #[test]
    fn seeds_stay_small() {
        let seed = Seed::around(&"a".repeat(300), (150, 150));
        assert_eq!((seed.start, seed.text.len()), (50, 200));
        // A long selection gets less context, and one past the cap none at all.
        let text = "b".repeat(5000);
        let seed = Seed::around(&text, (1000, 1990));
        assert_eq!((seed.start, seed.text.chars().count()), (995, 1000));
        assert!(Seed::around(&text, (0, 4000)) == Seed::default());
    }

    #[test]
    fn a_long_selection_is_replaced_from_an_empty_buffer() {
        let text = "c".repeat(30_000);
        let mut field = Field::new(&text, true, (0, 30_000));
        assert!(field.bridge.buffer().text.is_empty());
        let (bytes, _) = field.bridge.buffer().to_java();
        assert_eq!(&bytes[..], b"\0");
        field.keyboard("short", (5, 5));
        assert_eq!(field.text, "short");
    }

    #[test]
    fn a_cut_copy_ends_on_a_whole_character() {
        // 2-byte characters cut after an odd number of bytes.
        let two = "é".repeat(40_000);
        let cut = whole_chars(&two.as_bytes()[..copied(80_000)]);
        assert_eq!(cut.len(), NATIVE_BUFFER - 2);
        // Emoji are two 3-byte surrogates in modified UTF-8; 65535 = 6 * 10922 + 3
        // ends on a high surrogate, which goes.
        let emoji = Buffer {
            text: Zeroizing::new("😀".repeat(12_000)),
            selection: (0, 0),
        }
        .to_java()
        .0;
        let cut = whole_chars(&emoji[..copied(72_000)]);
        assert_eq!(cut.len(), 65_532);
        let buffer = Buffer::from_java(cut, (0, 0));
        assert!(!buffer.text.contains('\u{fffd}'));
        assert_eq!(buffer.text.chars().count(), 10_922);
        assert_eq!(whole_chars(b"ab"), b"ab");
    }

    #[test]
    fn leaving_a_secret_field_wipes_before_the_next_keyboard_type() {
        let mut focus = Focus::default();
        let (password, name) = (Id::unique("password"), Id::unique("name"));
        let (steps, moved) = focus.frame_end(Some(password), Some(Kind::Password));
        assert!(moved);
        assert_eq!(steps, [Step::CountActions, Step::Settings(Kind::Password)]);
        assert_eq!(focus.frame_end(Some(password), Some(Kind::Password)).0, []);
        // One frame: focus moves to a normal field, whose type is known already.
        let steps = focus.frame_end(Some(name), Some(Kind::Normal)).0;
        let at = |step: &Step| steps.iter().position(|s| s == step).unwrap();
        assert!(at(&Step::Wipe) < at(&Step::Settings(Kind::Normal)));
        // Normal to normal wipes nothing; a secret left for nothing is wiped.
        assert!(!focus.frame_end(None, None).0.contains(&Step::Wipe));
        focus.frame_end(Some(password), Some(Kind::Password));
        assert_eq!(
            focus.frame_end(None, None).0,
            [Step::Wipe, Step::CountActions]
        );
        // Its type not known yet in the frame focus moves: the wipe still comes first.
        focus.frame_end(Some(password), Some(Kind::Password));
        assert_eq!(
            focus.frame_end(Some(name), None).0,
            [Step::Wipe, Step::CountActions]
        );
        assert_eq!(
            focus.frame_end(Some(name), Some(Kind::Normal)).0,
            [Step::Settings(Kind::Normal)]
        );
        // Recovery words are secret too.
        let words = Id::unique("words");
        focus.frame_end(Some(words), Some(Kind::Private));
        let steps = focus.frame_end(Some(name), Some(Kind::Normal)).0;
        assert_eq!(
            steps,
            [Step::Wipe, Step::CountActions, Step::Settings(Kind::Normal)]
        );
    }

    #[test]
    fn typed_text_is_wiped_from_events() {
        let mut events = vec![Event::Text("secret".into()), Event::Paste("words".into())];
        wipe_text(&mut events);
        assert!(
            matches!(&events[..], [Event::Text(a), Event::Paste(b)] if a.is_empty() && b.is_empty())
        );
    }

    #[test]
    fn winits_input_event_dump_is_dropped_and_other_warnings_stay() {
        #[derive(Clone, Default)]
        struct Seen(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Seen {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut message = MessageAll::default();
                event.record(&mut message);
                self.0.lock().unwrap().push(message.0);
            }
        }
        #[derive(Default)]
        struct MessageAll(String);
        impl tracing::field::Visit for MessageAll {
            fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn fmt::Debug) {
                let _ = write!(self.0, "{value:?}");
            }
        }
        let seen = Seen::default();
        let subscriber = tracing_subscriber::registry()
            .with(NoTypedText)
            .with(seen.clone());
        tracing::subscriber::with_default(subscriber, || {
            let event = "TextEvent(TextInputState { text: \"hunter2\" })";
            tracing::warn!(target: "winit::platform_impl::android", "Unknown android_activity input event {event}");
            tracing::warn!(target: "winit::platform_impl::android", "Failed to get input events iterator: x");
            tracing::warn!(target: "egui_winit", "Unknown android_activity input event y");
        });
        let seen = seen.0.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen.iter().all(|line| !line.contains("hunter2")));
    }
}
