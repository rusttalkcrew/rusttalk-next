//! What winit 0.30 leaves out on Android (docs/mobile.md): typed text, the space
//! the keyboard and system bars cover, and ending with the activity.
//!
//! The on-screen keyboard doesn't send keys: it edits a text buffer that
//! GameActivity keeps, and winit drops the events that say it changed. Each frame
//! reads that buffer and turns what changed into egui text and IME events.

#[cfg(target_os = "android")]
pub use activity::*;

use eframe::egui::{Event, ImeEvent, Key, Modifiers};

/// What has already gone from the keyboard's buffer to egui.
#[derive(Default)]
pub struct Typing {
    /// The finished part of the buffer (everything but the word being composed).
    sent: String,
    /// egui shows a composed word (autocorrect, Japanese input) that isn't final.
    composing: bool,
}

impl Typing {
    /// Turn the buffer's new state into egui events. `compose` is the word being
    /// composed, in UTF-16 units as Java counts them. A newline becomes Enter.
    pub fn update(&mut self, text: &str, compose: Option<(usize, usize)>, events: &mut Vec<Event>) {
        let compose = compose
            .map(|(a, b)| (byte_index(text, a.min(b)), byte_index(text, a.max(b))))
            .filter(|(start, end)| start < end);
        let (committed, composed) = match compose {
            Some((start, end)) => ([&text[..start], &text[end..]].concat(), &text[start..end]),
            None => (text.to_owned(), ""),
        };
        let common = self
            .sent
            .char_indices()
            .zip(committed.chars())
            .find(|((_, sent), new)| sent != new)
            .map_or(self.sent.len().min(committed.len()), |((index, _), _)| {
                index
            });
        let removed = self.sent[common..].chars().count();
        if removed > 0 {
            if self.composing {
                events.push(preedit(""));
                self.composing = false;
            }
            events.push(Event::Ime(ImeEvent::DeleteSurrounding {
                before_chars: removed,
                after_chars: 0,
            }));
        }

        let added = &committed[common..];
        let mut lines = added.split('\n');
        let first = lines.next().unwrap_or_default();
        if self.composing && (!first.is_empty() || added.contains('\n') || composed.is_empty()) {
            // Replaces the composed word egui shows, also with nothing.
            events.push(Event::Ime(ImeEvent::Commit(first.to_owned())));
            self.composing = false;
        } else if !first.is_empty() {
            events.push(Event::Text(first.to_owned()));
        }
        for line in lines {
            for pressed in [true, false] {
                events.push(Event::Key {
                    key: Key::Enter,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                });
            }
            if !line.is_empty() {
                events.push(Event::Text(line.to_owned()));
            }
        }
        if !composed.is_empty() {
            events.push(preedit(composed));
            self.composing = true;
        }
        self.sent = committed;
    }

    pub fn composing(&self) -> bool {
        self.composing
    }
}

fn preedit(text: &str) -> Event {
    Event::Ime(ImeEvent::Preedit {
        text: text.to_owned(),
        active_range_chars: None,
    })
}

/// The byte offset of a UTF-16 offset, clamped to the text.
fn byte_index(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        if units >= utf16 {
            return index;
        }
        units += c.len_utf16();
    }
    text.len()
}

#[cfg(target_os = "android")]
mod activity {
    use std::{
        sync::{Mutex, OnceLock},
        time::Duration,
    };

    use anyhow::{Context as _, Result};
    use eframe::{
        UserEvent,
        egui::{self, Event, IMEPurpose, Id, Key, SafeAreaInsets, epaint::MarginF32},
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
                input::{ImeOptions, InputType, TextInputAction, TextInputState},
            },
        },
        window::WindowId,
    };

    use super::Typing;

    /// One activity per process: `android_main` ends the process after it.
    static APP: OnceLock<AndroidApp> = OnceLock::new();
    static KEYBOARD: Mutex<Keyboard> = Mutex::new(Keyboard::new());
    /// System bars, display cutout and keyboard, in pixels: left, top, right, bottom.
    static INSETS: Mutex<[i32; 4]> = Mutex::new([0; 4]);
    /// `WindowInsetsCompat.Type`: status bars 1, navigation bars 2, caption bar 4,
    /// keyboard 8, display cutout 128.
    const COVERED: i32 = 1 | 2 | 4 | 8 | 128;
    /// How often a frame looks at the keyboard's buffer while a field is focused:
    /// nothing tells the app that it changed.
    const TYPING_POLL: Duration = Duration::from_millis(40);

    struct Keyboard {
        typing: Typing,
        focused: Option<Id>,
        purpose: Option<IMEPurpose>,
        /// Insets at the last frame start, to bring the field back into view.
        insets: [i32; 4],
    }

    impl Keyboard {
        const fn new() -> Self {
            Self {
                typing: Typing {
                    sent: String::new(),
                    composing: false,
                },
                focused: None,
                purpose: None,
                insets: [0; 4],
            }
        }

        fn clear(&mut self, app: &AndroidApp) {
            self.typing = Typing::default();
            app.set_text_input_state(TextInputState::default());
        }
    }

    /// Run eframe until the activity is destroyed. winit 0.30 ignores `onDestroy`,
    /// which then waits for `android_main` to return.
    pub fn run(options: eframe::NativeOptions, creator: eframe::AppCreator<'_>) -> Result<()> {
        let app = options.android_app.clone().context("no Android activity")?;
        let _ = APP.set(app.clone());
        // GameTextInput starts without a text buffer, and reading that panics in
        // android-activity; this gives it one long before a field is focused.
        app.set_text_input_state(TextInputState::default());
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
            self.eframe.resumed(event_loop);
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
            self.eframe.user_event(event_loop, event);
        }

        fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
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
        // SAFETY: both pointers stay valid while `app` lives: android-activity
        // holds the VM and a global reference to the activity.
        let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
        vm.attach_current_thread(|env| {
            let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
            f(env, &activity)
        })
    }

    /// Ask the activity for its insets on its own thread (views aren't thread
    /// safe); a change paints a new frame.
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
                    let mut current = INSETS.lock().unwrap_or_else(|e| e.into_inner());
                    if *current != insets {
                        *current = insets;
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
        query_insets(app, ctx);
        let [left, top, right, bottom] = INSETS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|px| px as f32 / ctx.pixels_per_point());
        raw_input.safe_area_insets = Some(SafeAreaInsets(MarginF32 {
            left,
            right,
            top,
            bottom,
        }));

        let mut keyboard = KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
        let focused = ctx.memory(|memory| memory.focused());
        if focused != keyboard.focused {
            keyboard.focused = focused;
            keyboard.clear(app);
        }
        if ctx.egui_wants_keyboard_input() {
            let state = app.text_input_state();
            let compose = state.compose_region.map(|span| (span.start, span.end));
            keyboard
                .typing
                .update(&state.text, compose, &mut raw_input.events);
        }
        // A sent message empties its field: the keyboard's buffer starts over too.
        let enter = raw_input.events.iter().any(|event| {
            matches!(
                event,
                Event::Key {
                    key: Key::Enter,
                    pressed: true,
                    ..
                }
            )
        });
        if enter && !keyboard.typing.composing() {
            keyboard.clear(app);
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
        let mut keyboard = KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
        let insets = *INSETS.lock().unwrap_or_else(|e| e.into_inner());
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
        let Some(ime) = ctx.output(|output| output.ime) else {
            return;
        };
        ctx.request_repaint_after(TYPING_POLL);
        let mut keyboard = KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
        if ime.should_interrupt_composition {
            keyboard.clear(app);
        }
        if keyboard.purpose != Some(ime.purpose) {
            keyboard.purpose = Some(ime.purpose);
            // Multi-line: without it GameActivity drops the newline Enter types.
            let mut input = InputType::TYPE_CLASS_TEXT | InputType::TYPE_TEXT_FLAG_MULTI_LINE;
            // The keyboard needs no action key: Enter arrives as a newline.
            let mut options = ImeOptions::IME_FLAG_NO_ENTER_ACTION
                | ImeOptions::IMG_FLAG_NO_EXTRACT_UI
                | ImeOptions::IME_FLAG_NO_FULLSCREEN;
            if ime.purpose == IMEPurpose::Password {
                // No suggestions, and the keyboard must not learn passwords.
                input |= InputType::TYPE_TEXT_VARIATION_PASSWORD;
                options |= ImeOptions::IME_FLAG_NO_PERSONALIZED_LEARNING;
            }
            app.set_ime_editor_info(input, TextInputAction::None, options);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(typing: &mut Typing, text: &str, compose: Option<(usize, usize)>) -> Vec<Event> {
        let mut events = Vec::new();
        typing.update(text, compose, &mut events);
        events
    }

    fn commit(text: &str) -> Event {
        Event::Ime(ImeEvent::Commit(text.to_owned()))
    }

    #[test]
    fn plain_letters_become_text() {
        let mut typing = Typing::default();
        assert_eq!(typed(&mut typing, "h", None), [Event::Text("h".into())]);
        assert_eq!(typed(&mut typing, "hi", None), [Event::Text("i".into())]);
        assert_eq!(typed(&mut typing, "hi", None), []);
    }

    #[test]
    fn composed_words_show_until_committed() {
        let mut typing = Typing::default();
        assert_eq!(typed(&mut typing, "helo", Some((0, 4))), [preedit("helo")]);
        // Autocorrect replaces the word and a space finishes it.
        assert_eq!(typed(&mut typing, "hello ", None), [commit("hello ")]);
        assert_eq!(typed(&mut typing, "hello w", Some((6, 7))), [preedit("w")]);
        // Backspace inside the word, then all of it.
        assert_eq!(typed(&mut typing, "hello ", Some((6, 6))), [commit("")]);
        assert!(!typing.composing());
    }

    #[test]
    fn deleting_finished_text_deletes_before_the_cursor() {
        let mut typing = Typing::default();
        typed(&mut typing, "abc", None);
        assert_eq!(
            typed(&mut typing, "ab", None),
            [Event::Ime(ImeEvent::DeleteSurrounding {
                before_chars: 1,
                after_chars: 0
            })]
        );
        // Taking back a composed word that ended: the IME composes it again.
        typed(&mut typing, "ab cd", Some((3, 5)));
        assert_eq!(
            typed(&mut typing, "ab", Some((0, 2))),
            [
                preedit(""),
                Event::Ime(ImeEvent::DeleteSurrounding {
                    before_chars: 3,
                    after_chars: 0
                }),
                preedit("ab"),
            ]
        );
    }

    #[test]
    fn newline_is_enter() {
        let mut typing = Typing::default();
        let events = typed(&mut typing, "hi\n", None);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], Event::Text("hi".into()));
        assert!(matches!(
            events[1],
            Event::Key {
                key: Key::Enter,
                pressed: true,
                ..
            }
        ));
    }

    #[test]
    fn japanese_compose_region_counts_utf16_units() {
        let mut typing = Typing::default();
        // "😀" is two UTF-16 units; the composed reading follows it.
        assert_eq!(
            typed(&mut typing, "😀にほん", Some((2, 5))),
            [Event::Text("😀".into()), preedit("にほん")]
        );
        assert_eq!(typed(&mut typing, "😀日本", None), [commit("日本")]);
    }
}
