//! What winit 0.30 leaves out on Android (docs/mobile.md): typed text, the space
//! the keyboard and system bars cover, and ending with the activity.
//!
//! The on-screen keyboard doesn't send keys: it edits a text buffer that
//! GameActivity's GameTextInput keeps, and winit drops the events that say it
//! changed. The buffer stands for a stretch of the focused egui field: it starts
//! empty at the field's cursor (or for its selection), and each change the keyboard
//! makes to it becomes the same change, at the same place, in the field. Moving the
//! cursor in the field starts the buffer over there.

#[cfg(target_os = "android")]
pub use activity::*;

use eframe::egui::{
    Context, Event, Id, Key, Modifiers,
    text::{CCursor, CCursorRange},
    text_edit::TextEditState,
};
use zeroize::Zeroizing;

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

/// Turns changes of the keyboard's buffer into the same changes of the focused field.
#[derive(Default)]
pub struct Bridge {
    /// The buffer as last applied.
    last: Buffer,
    /// Where the buffer starts in the field, and how many of the field's characters
    /// it stands for: at first the selection, which its first change replaces.
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
    /// The buffer is empty again and stands for the field's selection.
    pub fn start(&mut self, cursor: (usize, usize)) {
        *self = Self {
            anchor: cursor.0,
            region: cursor.1 - cursor.0,
            expected: Some(cursor),
            ..Self::default()
        };
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
        if inserted == "\n" && start == end {
            // The Enter key types a lone newline (winit drops the keyboard's action
            // key); a pasted newline comes with other text and stays text.
            for pressed in [true, false] {
                events.push(Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                });
            }
            return Change {
                restart: true,
                ..Change::default()
            };
        }
        if !inserted.is_empty() {
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

/// The field's cursor or selection, in characters (sorted).
pub fn cursor(ctx: &Context, field: Id) -> Option<(usize, usize)> {
    let range = TextEditState::load(ctx, field)?.cursor.char_range()?;
    let [a, b] = range.sorted_cursors();
    Some((a.index.0, b.index.0))
}

pub fn set_cursor(ctx: &Context, field: Id, (a, b): (usize, usize)) {
    if let Some(mut state) = TextEditState::load(ctx, field) {
        let range = CCursorRange::two(CCursor::new(a), CCursor::new(b));
        state.cursor.set_char_range(Some(range));
        state.store(ctx, field);
    }
}

#[cfg(target_os = "android")]
mod activity {
    use std::{
        ffi::{c_char, c_void},
        slice,
        sync::{
            LazyLock, Mutex, MutexGuard, OnceLock,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    use anyhow::{Context as _, Result};
    use eframe::{
        UserEvent,
        egui::{self, IMEPurpose, Id, SafeAreaInsets, epaint::MarginF32},
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
                input::{ImeOptions, InputType, TextInputAction},
            },
        },
        window::WindowId,
    };

    use super::{Bridge, Buffer, cursor, set_cursor};

    /// One activity per process: `android_main` ends the process after it.
    static APP: OnceLock<AndroidApp> = OnceLock::new();
    static KEYBOARD: LazyLock<Mutex<Keyboard>> = LazyLock::new(Mutex::default);
    /// The buffer as last read on the Java thread, after start-over number `.0`.
    static READ: Mutex<Option<(u64, Buffer)>> = Mutex::new(None);
    /// System bars, display cutout and keyboard, in pixels: left, top, right, bottom.
    static INSETS: Mutex<[i32; 4]> = Mutex::new([0; 4]);
    /// The window or the keyboard changed: follow the insets for a moment.
    static INSETS_STALE: AtomicBool = AtomicBool::new(true);
    /// `WindowInsetsCompat.Type`: status bars 1, navigation bars 2, caption bar 4,
    /// keyboard 8, display cutout 128.
    const COVERED: i32 = 1 | 2 | 4 | 8 | 128;
    /// How long insets are read every frame after a change (the keyboard slides).
    const INSETS_SETTLE: Duration = Duration::from_millis(600);
    /// While the keyboard is up it can be closed with Back, which tells the app nothing.
    const KEYBOARD_CHECK: Duration = Duration::from_millis(500);
    /// How often a frame reads the keyboard's buffer while a field is focused:
    /// nothing tells the app that it changed.
    const TYPING_POLL: Duration = Duration::from_millis(40);
    /// How long a destroyed activity waits for messages still being sent.
    const SEND_GRACE: Duration = Duration::from_secs(3);
    /// GameTextInput's text buffer (`DEFAULT_MAX_STRING_SIZE`), which never shrinks.
    const NATIVE_BUFFER: usize = 1 << 16;

    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Normal,
        /// Recovery words: shown, but no suggestions and no learning.
        Private,
        Password,
    }

    struct Keyboard {
        bridge: Bridge,
        focused: Option<Id>,
        /// Start-overs asked for; the bridge waits until the Java thread did the latest.
        generation: u64,
        waiting: bool,
        /// A secret field had focus since the last start-over: wipe the buffer.
        secret: bool,
        kind: Option<Kind>,
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
                focused: None,
                generation: 0,
                waiting: false,
                secret: false,
                kind: None,
                ime: false,
                insets_until: Instant::now(),
                insets_next: Instant::now(),
                insets: [0; 4],
            }
        }
    }

    impl Keyboard {
        fn restart(&mut self, app: &AndroidApp, ctx: &egui::Context) {
            self.generation += 1;
            self.waiting = true;
            let (generation, wipe) = (self.generation, std::mem::take(&mut self.secret));
            on_java_thread(app, ctx, generation, move |input| {
                if wipe {
                    // Overwrite all the secret ever used, then empty it.
                    let mut filler = vec![b'x'; NATIVE_BUFFER - 1];
                    filler.push(0);
                    // SAFETY: `input` is live on this thread; the text ends in NUL.
                    unsafe { set_state(input, &filler) };
                }
                // SAFETY: as above.
                unsafe { set_state(input, b"\0") };
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
        let _ = APP.set(app.clone());
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
                // The process ends next; the app's tasks don't get to finish.
                crate::net::wait_for_sends(SEND_GRACE);
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

    fn java<T>(
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

    /// Set the buffer; `text` is modified UTF-8 ending in NUL (ASCII here).
    ///
    /// SAFETY: `input` is GameTextInput, used on the Java thread.
    unsafe fn set_state(input: *mut c_void, text: &[u8]) {
        let length = i32::try_from(text.len() - 1).unwrap_or(0);
        let state = State {
            text: text.as_ptr().cast(),
            length,
            selection: Span {
                start: length,
                end: length,
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
            let length = usize::try_from(state.length).unwrap_or(0);
            // The text is null before the first state is set.
            let bytes = match state.text.is_null() || length == 0 {
                true => &[][..],
                false => unsafe { slice::from_raw_parts(state.text.cast::<u8>(), length) },
            };
            let selection = (state.selection.start, state.selection.end);
            *out = Some(Buffer::from_java(bytes, selection));
        }
        let mut out = None;
        unsafe { GameTextInput_getState(input, copy, (&raw mut out).cast()) };
        out
    }

    /// Run `f` with GameTextInput on the Java thread, where the keyboard edits it,
    /// then read the buffer back as after start-over `generation`.
    fn on_java_thread(
        app: &AndroidApp,
        ctx: &egui::Context,
        generation: u64,
        f: impl FnOnce(*mut c_void) + Send + 'static,
    ) {
        let (main, ctx) = (app.clone(), ctx.clone());
        app.run_on_java_main_thread(Box::new(move || {
            let handle = java(&main, |env, activity| {
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
                    return Ok(0);
                }
                env.get_static_field(&class, jni_str!("nativeHandle"), jni_sig!("J"))?
                    .j()
            });
            let handle = match handle {
                Ok(0) => return,
                Ok(handle) => handle,
                Err(error) => return tracing::warn!(%error, "find the keyboard's text"),
            };
            // SAFETY: the live GameActivity; it is only freed on this thread.
            let input = unsafe { GameActivity_getTextInput(handle as *const c_void) };
            if input.is_null() {
                return;
            }
            f(input);
            // SAFETY: as above.
            let Some(buffer) = (unsafe { read_state(input) }) else {
                return;
            };
            let mut read = lock(&READ);
            if read.as_ref() != Some(&(generation, buffer.clone())) {
                *read = Some((generation, buffer));
                ctx.request_repaint();
            }
        }));
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

        let focused = ctx.memory(|memory| memory.focused());
        if focused != keyboard.focused {
            keyboard.focused = focused;
            keyboard.restart(app, ctx);
        }
        let Some(field) = focused.filter(|_| ctx.egui_wants_keyboard_input()) else {
            return;
        };
        let Some(cursor) = cursor(ctx, field) else {
            return;
        };
        let read = lock(&READ).clone();
        if let Some((_, buffer)) = read.filter(|(generation, _)| *generation == keyboard.generation)
        {
            if keyboard.waiting {
                // The buffer is empty now, and only the keyboard changes it from here.
                keyboard.waiting = false;
                keyboard.bridge.start(cursor);
            }
            let change = keyboard
                .bridge
                .update(cursor, &buffer, &mut raw_input.events);
            if let Some(range) = change.cursor {
                set_cursor(ctx, field, range);
            }
            if change.restart {
                return keyboard.restart(app, ctx);
            }
        }
        if !keyboard.waiting {
            on_java_thread(app, ctx, keyboard.generation, |_| {});
        }
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
            if let Some(field) = keyboard.focused.and_then(|id| ctx.read_response(id)) {
                field.scroll_to_me(None);
            }
        }
    }

    /// At the end of the frame: tell the keyboard what the focused field is for.
    fn frame_end(ctx: &egui::Context) {
        let Some(app) = APP.get() else { return };
        let mut keyboard = lock(&KEYBOARD);
        let ime = ctx.output(|output| output.ime);
        if ime.is_some() != keyboard.ime {
            keyboard.ime = ime.is_some();
            INSETS_STALE.store(true, Ordering::Relaxed);
        }
        if Instant::now() < keyboard.insets_until {
            ctx.request_repaint_after(TYPING_POLL);
        }
        let Some(ime) = ime else { return };
        ctx.request_repaint_after(TYPING_POLL);
        let private = ctx
            .memory(|memory| memory.focused())
            .is_some_and(|field| crate::ui::is_private(ctx, field));
        let kind = match ime.purpose {
            IMEPurpose::Password => Kind::Password,
            _ if private => Kind::Private,
            _ => Kind::Normal,
        };
        keyboard.secret |= kind != Kind::Normal;
        if keyboard.kind == Some(kind) {
            return;
        }
        keyboard.kind = Some(kind);
        // Multi-line: without it GameActivity drops the newline Enter types.
        let mut input = InputType::TYPE_CLASS_TEXT | InputType::TYPE_TEXT_FLAG_MULTI_LINE;
        // The keyboard needs no action key: Enter arrives as a newline.
        let mut options = ImeOptions::IME_FLAG_NO_ENTER_ACTION
            | ImeOptions::IMG_FLAG_NO_EXTRACT_UI
            | ImeOptions::IME_FLAG_NO_FULLSCREEN;
        if kind != Kind::Normal {
            // No suggestions, and the keyboard must not learn it.
            input |= match kind {
                Kind::Password => InputType::TYPE_TEXT_VARIATION_PASSWORD,
                _ => InputType::TYPE_TEXT_VARIATION_VISIBLE_PASSWORD,
            } | InputType::TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            options |= ImeOptions::IME_FLAG_NO_PERSONALIZED_LEARNING;
        }
        app.set_ime_editor_info(input, TextInputAction::None, options);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{RawInput, TextEdit};

    const FIELD: &str = "field";

    /// A focused egui field and the bridge in front of it.
    struct Field {
        ctx: Context,
        text: String,
        multiline: bool,
        bridge: Bridge,
    }

    impl Field {
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
            field.bridge.start(field.cursor());
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
    fn composing_in_the_middle_keeps_the_text_after_it() {
        // "a|b": Japanese input composes にほん between them, then converts it.
        let mut field = Field::new("ab", false, (1, 1));
        field.keyboard("に", (1, 1));
        field.keyboard("にほん", (3, 3));
        field.keyboard("日本", (2, 2));
        assert_eq!(field.text, "a日本b");
        assert_eq!(field.cursor(), (3, 3));
    }

    #[test]
    fn an_emoji_before_the_change_counts_as_one_character() {
        let mut field = Field::new("😀ab", false, (2, 2));
        field.keyboard("x", (1, 1));
        field.keyboard("xy", (2, 2));
        assert_eq!(field.text, "😀axyb");
        assert_eq!(field.cursor(), (4, 4));
    }

    #[test]
    fn typing_replaces_the_selection() {
        let mut field = Field::new("hello world", false, (6, 11));
        field.keyboard("there", (5, 5));
        assert_eq!(field.text, "hello there");
        assert_eq!(field.cursor(), (11, 11));
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
        field.keyboard("abc", (3, 3));
        // The keyboard selects "bc" (its composed part) and deletes it.
        field.keyboard("abc", (1, 3));
        assert_eq!(field.cursor(), (2, 4));
        field.keyboard("a", (1, 1));
        assert_eq!(field.text, "xay");
        assert_eq!(field.cursor(), (2, 2));
    }

    #[test]
    fn the_keyboard_moving_its_cursor_moves_the_fields() {
        let mut field = Field::new("[]", false, (1, 1));
        field.keyboard("abc", (3, 3));
        field.keyboard("abc", (1, 1));
        field.keyboard("aXbc", (2, 2));
        assert_eq!(field.text, "[aXbc]");
        assert_eq!(field.cursor(), (3, 3));
    }

    #[test]
    fn enter_presses_enter_but_pasted_newlines_stay_text() {
        let mut field = Field::new("", true, (0, 0));
        field.keyboard("a\nb", (3, 3));
        assert_eq!(field.text, "a\nb");
        let mut events = Vec::new();
        let buffer = Buffer {
            text: Zeroizing::new("a\nb\n".to_owned()),
            selection: (4, 4),
        };
        let change = field.bridge.update(field.cursor(), &buffer, &mut events);
        assert!(change.restart);
        assert!(matches!(
            events[..],
            [
                Event::Key {
                    key: Key::Enter,
                    pressed: true,
                    ..
                },
                Event::Key { pressed: false, .. }
            ]
        ));
    }

    #[test]
    fn moving_the_cursor_in_the_field_starts_over() {
        let mut field = Field::new("abc", false, (3, 3));
        field.keyboard("d", (1, 1));
        set_cursor(&field.ctx, Id::unique(FIELD), (0, 0));
        assert!(field.keyboard("de", (2, 2)).restart);
        assert_eq!(field.text, "abcd");
    }

    #[test]
    fn java_text_and_selection() {
        // "a", NUL as C0 80, 😀 as two 3-byte surrogates, then "b".
        let bytes = [b'a', 0xc0, 0x80, 0xed, 0xa0, 0xbd, 0xed, 0xb8, 0x80, b'b'];
        // UTF-16: a=0, NUL=1, 😀=2..4, b=4.
        let buffer = Buffer::from_java(&bytes, (4, 2));
        assert_eq!(&*buffer.text, "a\0😀b");
        assert_eq!(buffer.selection, (2, 3));
        assert_eq!(Buffer::from_java(b"ab", (-1, -1)).selection, (2, 2));
        assert_eq!(&*Buffer::from_java(&[0xff, b'a'], (0, 0)).text, "\u{fffd}a");
    }
}
