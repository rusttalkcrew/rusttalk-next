//! Phones: the system clipboard, and a long-press menu on text fields to use it.
//! egui-winit's clipboard is only a string inside the app on Android and iOS
//! (arboard has no phone backend), so copied text never left the app and nothing
//! from other apps could be pasted. Never log clipboard contents.
// Tests build it on desktops, which use only part of it.
#![cfg_attr(not(any(target_os = "android", target_os = "ios")), allow(dead_code))]

use std::sync::Mutex;

use eframe::egui::{
    self, Context, Event, Id, Key, Modifiers, OutputCommand, Popup, RawInput,
    text::CCursorRange,
    text_edit::{TextEditOutput, TextEditState},
};
use zeroize::Zeroizing;

use crate::ui;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Paste,
    Copy,
    Cut,
    SelectAll,
}

/// A picked action, run at the start of the next frame for `field` (`None`: a
/// paste for the screen itself, see `paste_for_screen`).
static PENDING: Mutex<Option<(Option<Id>, Action)>> = Mutex::new(None);

fn menu_open_id() -> Id {
    Id::unique("phone clipboard menu open")
}

/// The menu of a text field, as on phones: a long press anywhere in it (egui's
/// secondary click), or a tap on the cursor of a field that already had focus.
/// Copy and Cut only with a selection, and never out of a `secret` field.
pub fn menu(output: &TextEditOutput, secret: bool) {
    let response = &output.response.response;
    let ctx = &response.ctx;
    keep_keyboard(response);
    // Pressing a field moves its cursor, so a long press keeps the selection from
    // before the press, to copy or cut it. Kept only while the field has focus, so
    // it also tells whether the field had focus before the press.
    let before = response.id.with("selection before press");
    let mut state = TextEditState::load(ctx, response.id);
    let range = state.as_ref().and_then(|state| state.cursor.char_range());
    let kept = ctx.data(|data| data.get_temp::<CCursorRange>(before));
    let long_press = response.long_touched();
    // A tap that left the cursor where it was.
    let tap_on_cursor = response.clicked()
        && !long_press
        && kept.is_some_and(|kept| kept.is_empty() && Some(kept) == range);
    if long_press {
        if let Some(state) = &mut state
            && let Some(kept) = kept
            && !kept.is_empty()
        {
            state.cursor.set_char_range(Some(kept));
            state.clone().store(ctx, response.id);
        }
    } else if !response.has_focus() {
        ctx.data_mut(|data| data.remove_temp::<CCursorRange>(before));
    } else if !response.is_pointer_button_down_on()
        && let Some(range) = range
    {
        ctx.data_mut(|data| data.insert_temp(before, range));
    }
    #[cfg(target_os = "ios")]
    if response.clicked() || long_press {
        // For the device log: whether iOS delivers the press and hold (no text).
        let held = ctx.input(|input| {
            (input.time - input.pointer.press_start_time().unwrap_or(input.time)) * 1000.0
        });
        tracing::info!(
            held_ms = held as u64,
            long_press,
            tap_on_cursor,
            "text field touch"
        );
    }
    // From the stored state: the rows must not change between pressing one and
    // letting go.
    let selected = state
        .and_then(|state| state.cursor.char_range())
        .is_some_and(|range| !range.is_empty());
    if Popup::is_id_open(ctx, Popup::default_response_id(response)) {
        let pass = ctx.cumulative_pass_nr();
        ctx.data_mut(|data| data.insert_temp(menu_open_id(), pass));
    }
    let picked = ui::touch_menu(response, tap_on_cursor, |m| {
        let mut picked = None;
        let mut row = |m: &mut ui::Menu, icon, label: &str, action| {
            if m.item(icon, label).clicked() {
                picked = Some(action);
            }
        };
        row(m, "📋", t!("clipboard.paste"), Action::Paste);
        if selected && !secret {
            row(m, "🗐", t!("clipboard.copy"), Action::Copy);
            row(m, "✂", t!("clipboard.cut"), Action::Cut);
        }
        row(m, "⛶", t!("clipboard.select_all"), Action::SelectAll);
        picked
    })
    .flatten();
    if let Some(action) = picked {
        if action == Action::Paste {
            system::read(ctx);
        }
        *lock() = Some((Some(response.id), action));
        ctx.request_repaint();
    }
}

fn lock() -> std::sync::MutexGuard<'static, Option<(Option<Id>, Action)>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Paste the system clipboard into the focused field (a hardware keyboard's paste).
pub fn paste_focused(ctx: &Context) {
    if let Some(field) = ctx.memory(|memory| memory.focused()) {
        system::read(ctx);
        *lock() = Some((Some(field), Action::Paste));
    }
}

/// A paste the screen takes itself (`Event::Paste` without moving focus), like the
/// recovery words' Paste button.
pub fn paste_for_screen(ctx: &Context) {
    system::read(ctx);
    *lock() = Some((None, Action::Paste));
}

/// Copy recovery words: marked sensitive, so the system doesn't show them.
pub fn copy_secret(text: Zeroizing<String>) {
    system::write(text, true);
}

/// Before the frame: the picked action as the field's own events. Runs after the
/// keyboard's changes, so a paste lands where the cursor ends up; the cursor then
/// moved, which starts the keyboard's buffer over with the pasted text.
pub fn input(ctx: &Context, raw_input: &mut RawInput) {
    // While the menu is open, picking a row must not take the field's focus (and
    // close the keyboard).
    let open = ctx.data(|data| data.get_temp::<u64>(menu_open_id()))
        == Some(ctx.cumulative_pass_nr().saturating_sub(1));
    ctx.options_mut(|options| {
        options.input_options.surrender_focus_on = match open {
            true => egui::SurrenderFocusOn::Never,
            false => egui::SurrenderFocusOn::default(),
        }
    });

    let mut pending = lock();
    let Some((field, action)) = *pending else {
        return;
    };
    let event = match action {
        Action::Paste => match system::take() {
            // Still on its way from the Java thread.
            None => return,
            Some(None) => {
                *pending = None;
                return;
            }
            Some(Some(text)) => Event::Paste(text.to_string()),
        },
        Action::Copy => Event::Copy,
        Action::Cut => Event::Cut,
        Action::SelectAll => Event::Key {
            key: Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        },
    };
    *pending = None;
    if let Some(field) = field {
        ctx.memory_mut(|memory| memory.request_focus(field));
    }
    raw_input.events.push(event);
}

/// Tapping a text field asks egui for focus (again, when it has it), which also
/// interrupts the keyboard's typing: egui-winit then turns the keyboard off and on
/// (winit hides and shows Android's, and resigns and becomes first responder on
/// iOS), so it closes and opens again. Phones gain nothing from it: iOS has no
/// composing in winit, and on Android `android::Bridge` starts the keyboard's
/// buffer over itself. egui forgets the request at the start of a pass, so such a
/// frame runs another pass without input, which draws the same and keeps the
/// keyboard up.
fn keep_keyboard(response: &egui::Response) {
    let ctx = &response.ctx;
    let touched = response.clicked()
        || response.drag_started()
        || ctx.input(|input| input.pointer.any_pressed()) && response.is_pointer_button_down_on();
    if touched && response.has_focus() {
        ctx.request_discard("tap on a focused field");
        // A tap on the field still brings back a keyboard that was put away (with
        // Back on Android); showing a keyboard that is up changes nothing.
        #[cfg(target_os = "android")]
        if let Some(app) = crate::android::app() {
            app.show_soft_input(true);
        }
        #[cfg(target_os = "ios")]
        crate::ios::show_keyboard();
    }
}

/// At the end of the frame: text the app copied goes to the system clipboard.
pub fn frame_end(ctx: &Context) {
    // Nothing repaints while a finger rests still (iOS sends no touches then), and
    // egui only sees a press and hold on a frame after it.
    if ctx.input(|input| input.any_touches()) {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    let mut copied = None;
    ctx.output_mut(|output| {
        output.commands.retain_mut(|command| match command {
            OutputCommand::CopyText(text) => {
                copied = Some(Zeroizing::new(std::mem::take(text)));
                false
            }
            _ => true,
        });
    });
    if let Some(text) = copied {
        system::write(text, false);
    }
}

#[cfg(target_os = "android")]
mod system {
    use std::sync::Mutex;

    use eframe::egui::Context;
    use jni::{JValue, jni_sig, jni_str, objects::JString};
    use zeroize::Zeroizing;

    use crate::android::{app, java};

    /// The clipboard's text as read on the Java thread; `Some(None)` when it held none.
    static READ: Mutex<Option<Option<Zeroizing<String>>>> = Mutex::new(None);

    fn lock() -> std::sync::MutexGuard<'static, Option<Option<Zeroizing<String>>>> {
        READ.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read the clipboard on the Java thread; Android lets only the focused app read it.
    pub fn read(ctx: &Context) {
        *lock() = None;
        let Some(app) = app() else {
            *lock() = Some(None);
            return;
        };
        let (main, ctx) = (app.clone(), ctx.clone());
        app.run_on_java_main_thread(Box::new(move || {
            let text = java(&main, |env, activity| {
                let service = env.new_string("clipboard")?;
                let manager = env
                    .call_method(
                        activity,
                        jni_str!("getSystemService"),
                        jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                        &[JValue::Object(&service)],
                    )?
                    .l()?;
                let clip = env
                    .call_method(
                        &manager,
                        jni_str!("getPrimaryClip"),
                        jni_sig!("()Landroid/content/ClipData;"),
                        &[],
                    )?
                    .l()?;
                if clip.is_null()
                    || env
                        .call_method(&clip, jni_str!("getItemCount"), jni_sig!("()I"), &[])?
                        .i()?
                        == 0
                {
                    return Ok(None);
                }
                let item = env
                    .call_method(
                        &clip,
                        jni_str!("getItemAt"),
                        jni_sig!("(I)Landroid/content/ClipData$Item;"),
                        &[JValue::Int(0)],
                    )?
                    .l()?;
                // Also text from a copied link or styled text.
                let text = env
                    .call_method(
                        &item,
                        jni_str!("coerceToText"),
                        jni_sig!("(Landroid/content/Context;)Ljava/lang/CharSequence;"),
                        &[JValue::Object(activity)],
                    )?
                    .l()?;
                if text.is_null() {
                    return Ok(None);
                }
                let text = env
                    .call_method(
                        &text,
                        jni_str!("toString"),
                        jni_sig!("()Ljava/lang/String;"),
                        &[],
                    )?
                    .l()?;
                let text = env.cast_local::<JString>(text)?;
                Ok(Some(Zeroizing::new(text.try_to_string(env)?)))
            });
            let text = text.unwrap_or_else(|error| {
                tracing::warn!(%error, "reading the clipboard failed");
                None
            });
            *lock() = Some(text.filter(|text| !text.is_empty()));
            ctx.request_repaint();
        }));
    }

    pub fn take() -> Option<Option<Zeroizing<String>>> {
        lock().take()
    }

    /// `sensitive`: the clipboard preview and keyboard suggestions don't show it
    /// (`ClipDescription.EXTRA_IS_SENSITIVE`, Android 13 and later).
    pub fn write(text: Zeroizing<String>, sensitive: bool) {
        let Some(app) = app() else { return };
        let main = app.clone();
        app.run_on_java_main_thread(Box::new(move || {
            let written = java(&main, |env, activity| {
                let service = env.new_string("clipboard")?;
                let manager = env
                    .call_method(
                        activity,
                        jni_str!("getSystemService"),
                        jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
                        &[JValue::Object(&service)],
                    )?
                    .l()?;
                let label = env.new_string("RustTalk")?;
                let text = env.new_string(&*text)?;
                let clip = env
                    .call_static_method(
                        jni_str!("android/content/ClipData"),
                        jni_str!("newPlainText"),
                        jni_sig!(
                            "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;"
                        ),
                        &[JValue::Object(&label), JValue::Object(&text)],
                    )?
                    .l()?;
                if sensitive {
                    let description = env
                        .call_method(
                            &clip,
                            jni_str!("getDescription"),
                            jni_sig!("()Landroid/content/ClipDescription;"),
                            &[],
                        )?
                        .l()?;
                    let extras = env.new_object(
                        jni_str!("android/os/PersistableBundle"),
                        jni_sig!("()V"),
                        &[],
                    )?;
                    let key = env.new_string("android.content.extra.IS_SENSITIVE")?;
                    env.call_method(
                        &extras,
                        jni_str!("putBoolean"),
                        jni_sig!("(Ljava/lang/String;Z)V"),
                        &[JValue::Object(&key), JValue::Bool(true)],
                    )?;
                    env.call_method(
                        &description,
                        jni_str!("setExtras"),
                        jni_sig!("(Landroid/os/PersistableBundle;)V"),
                        &[JValue::Object(&extras)],
                    )?;
                }
                env.call_method(
                    &manager,
                    jni_str!("setPrimaryClip"),
                    jni_sig!("(Landroid/content/ClipData;)V"),
                    &[JValue::Object(&clip)],
                )?;
                Ok(())
            });
            if let Err(error) = written {
                tracing::warn!(%error, "copying to the clipboard failed");
            }
        }));
    }
}

#[cfg(target_os = "ios")]
mod system {
    use eframe::egui::Context;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSArray, NSDate, NSDictionary, NSNumber, NSString};
    use objc2_ui_kit::{
        UIPasteboard, UIPasteboardOptionExpirationDate, UIPasteboardOptionLocalOnly,
    };
    use zeroize::Zeroizing;

    /// How long a sensitive copy stays on the pasteboard.
    const EXPIRES_SECONDS: f64 = 120.0;

    /// Nothing to start: the pasteboard is read on this (the main) thread in `take`.
    pub fn read(_: &Context) {}

    /// iOS may ask the person first whether the app may paste.
    pub fn take() -> Option<Option<Zeroizing<String>>> {
        // SAFETY: eframe runs on the main thread, where UIKit wants the pasteboard.
        let text = unsafe { UIPasteboard::generalPasteboard().string() };
        Some(
            text.map(|text| Zeroizing::new(text.to_string()))
                .filter(|text| !text.is_empty()),
        )
    }

    /// `sensitive`: only on this iPhone (no Universal Clipboard to a Mac nearby),
    /// and gone after two minutes.
    pub fn write(text: Zeroizing<String>, sensitive: bool) {
        let text = NSString::from_str(&text);
        let pasteboard = UIPasteboard::generalPasteboard();
        if !sensitive {
            // SAFETY: as in `take`.
            unsafe { pasteboard.setString(Some(&text)) };
            return;
        }
        let kind = NSString::from_str("public.utf8-plain-text");
        let value: &AnyObject = &text;
        let item = NSDictionary::<NSString, AnyObject>::from_slices(&[&*kind], &[value]);
        let items = NSArray::from_retained_slice(&[item]);
        let local = NSNumber::new_bool(true);
        let expires = NSDate::dateWithTimeIntervalSinceNow(EXPIRES_SECONDS);
        let (local, expires): (&AnyObject, &AnyObject) = (&local, &expires);
        // SAFETY: UIKit's own option names, read on the main thread.
        let keys = unsafe {
            [
                UIPasteboardOptionLocalOnly,
                UIPasteboardOptionExpirationDate,
            ]
        };
        let options = NSDictionary::<NSString, AnyObject>::from_slices(&keys, &[local, expires]);
        // SAFETY: as in `take`; the item holds a string, the options a bool and a date.
        unsafe { pasteboard.setItems_options(&items, &options) };
    }
}

/// Tests: a clipboard inside the process.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod system {
    use std::sync::Mutex;

    use eframe::egui::Context;
    use zeroize::Zeroizing;

    pub static CLIPBOARD: Mutex<Option<String>> = Mutex::new(None);
    /// Whether the last copy was marked sensitive.
    pub static SENSITIVE: Mutex<bool> = Mutex::new(false);

    pub fn read(_: &Context) {}

    pub fn take() -> Option<Option<Zeroizing<String>>> {
        Some(CLIPBOARD.lock().unwrap().clone().map(Zeroizing::new))
    }

    pub fn write(text: Zeroizing<String>, sensitive: bool) {
        *CLIPBOARD.lock().unwrap() = Some(text.to_string());
        *SENSITIVE.lock().unwrap() = sensitive;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::TextEdit;

    /// A focused field, a frame through `input` and `frame_end` as on a phone.
    fn frame(ctx: &Context, text: &mut String) {
        let mut raw = RawInput::default();
        input(ctx, &mut raw);
        let mut output = ctx.run_ui(raw, |ui| {
            let output = TextEdit::singleline(text).id(Id::unique("field")).show(ui);
            menu(&output, false);
            frame_end(ui.ctx());
        });
        output.textures_delta.clear();
    }

    #[test]
    fn picked_actions_use_the_system_clipboard() {
        let ctx = Context::default();
        let field = Id::unique("field");
        let mut text = "hello world".to_owned();
        ctx.memory_mut(|memory| memory.request_focus(field));
        frame(&ctx, &mut text);

        *lock() = Some((Some(field), Action::SelectAll));
        frame(&ctx, &mut text);
        *lock() = Some((Some(field), Action::Copy));
        frame(&ctx, &mut text);
        assert_eq!(
            system::CLIPBOARD.lock().unwrap().as_deref(),
            Some("hello world")
        );
        assert!(!*system::SENSITIVE.lock().unwrap());
        // Recovery words are marked sensitive (Android) or local and expiring (iOS).
        copy_secret(Zeroizing::new("apple banana".to_owned()));
        assert!(*system::SENSITIVE.lock().unwrap());
        assert_eq!(
            system::CLIPBOARD.lock().unwrap().as_deref(),
            Some("apple banana")
        );

        *system::CLIPBOARD.lock().unwrap() = Some("pasted".to_owned());
        *lock() = Some((Some(field), Action::Paste));
        frame(&ctx, &mut text);
        assert_eq!(text, "pasted");
        assert!(lock().is_none());
    }

    /// Frames of a focused, empty field; `tap` presses at frame 3 and lets go at 4.
    fn tap_frames(ctx: &Context, focused: bool) -> Vec<egui::FullOutput> {
        let field = Id::unique("tapped");
        let mut text = String::new();
        let mut outputs = Vec::new();
        for frame in 0..6 {
            let pos = egui::pos2(20.0, 10.0);
            let mut raw = RawInput {
                time: Some(frame as f64 * 0.05),
                ..RawInput::default()
            };
            if frame == 3 || frame == 4 {
                raw.events.push(Event::PointerMoved(pos));
                raw.events.push(Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: frame == 3,
                    modifiers: Modifiers::NONE,
                });
            }
            let mut output = ctx.run_ui(raw, |ui| {
                let output = TextEdit::singleline(&mut text).id(field).show(ui);
                menu(&output, false);
                if focused && frame == 0 {
                    output.response.response.request_focus();
                }
            });
            output.textures_delta.clear();
            outputs.push(output);
        }
        outputs
    }

    #[test]
    fn tapping_a_focused_field_keeps_the_keyboard_and_opens_the_menu() {
        let ctx = Context::default();
        let outputs = tap_frames(&ctx, true);
        for output in &outputs[3..5] {
            let ime = output.platform_output.ime.as_ref().expect("still typing");
            // egui-winit would turn the keyboard off and on for this.
            assert!(!ime.should_interrupt_composition);
            assert_eq!(output.platform_output.num_completed_passes, 2);
        }
        // The cursor stayed where it was: the menu opens, as on phones.
        let popup = Id::unique("tapped").with("popup");
        assert!(Popup::is_id_open(&ctx, popup));

        // The first tap on a field only gives it focus.
        let ctx = Context::default();
        tap_frames(&ctx, false);
        assert!(ctx.memory(|memory| memory.has_focus(Id::unique("tapped"))));
        assert!(!Popup::is_id_open(&ctx, popup));
    }

    #[test]
    fn long_press_opens_the_menu() {
        use eframe::egui::{TouchDeviceId, TouchId, TouchPhase, pos2};
        let ctx = Context::default();
        let mut text = "hello".to_owned();
        let touch = |phase, time: f64| {
            let mut raw = RawInput {
                time: Some(time),
                ..RawInput::default()
            };
            raw.events.push(Event::Touch {
                device_id: TouchDeviceId(0),
                id: TouchId(0),
                phase,
                pos: pos2(20.0, 10.0),
                force: None,
            });
            raw.events.push(match phase {
                TouchPhase::Start => Event::PointerButton {
                    pos: pos2(20.0, 10.0),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
                _ => Event::PointerMoved(pos2(20.0, 10.0)),
            });
            raw
        };
        let mut run = |raw: RawInput| {
            let mut output = ctx.run_ui(raw, |ui| {
                let output = TextEdit::singleline(&mut text)
                    .id(Id::unique("long"))
                    .show(ui);
                menu(&output, false);
            });
            output.textures_delta.clear();
        };
        ctx.memory_mut(|memory| memory.request_focus(Id::unique("long")));
        run(RawInput {
            time: Some(0.0),
            ..RawInput::default()
        });
        run(RawInput {
            time: Some(0.05),
            ..RawInput::default()
        });
        crate::android::set_cursor(&ctx, Id::unique("long"), (0, 5));
        run(RawInput {
            time: Some(0.08),
            ..RawInput::default()
        });
        run(touch(TouchPhase::Start, 0.1));
        run(RawInput {
            time: Some(0.5),
            ..RawInput::default()
        });
        run(RawInput {
            time: Some(1.0),
            ..RawInput::default()
        });
        run(RawInput {
            time: Some(1.1),
            ..RawInput::default()
        });
        let id = Id::unique("long").with("popup");
        assert!(Popup::is_id_open(&ctx, id));
        // The press moved the cursor; the long press brought the selection back.
        assert_eq!(
            crate::android::cursor(&ctx, Id::unique("long")),
            Some((0, 5))
        );
    }

    #[test]
    fn private_fields_never_copy_or_cut() {
        let ctx = Context::default();
        let mut words = "apple banana cherry".to_owned();
        let frame = |words: &mut String, events: Vec<Event>| {
            let mut raw = RawInput {
                events,
                ..RawInput::default()
            };
            // A hardware keyboard's Ctrl+C and Ctrl+X become Copy and Cut here.
            crate::paste::keys(&ctx, &mut raw);
            let mut output = ctx.run_ui(raw, |ui| {
                let field = ui.add(ui::field(words).id(Id::unique("words")).private());
                field.request_focus();
            });
            output.textures_delta.clear();
            output
                .platform_output
                .commands
                .iter()
                .any(|command| matches!(command, OutputCommand::CopyText(_)))
        };
        let key = |key| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        };
        frame(&mut words, Vec::new());
        frame(&mut words, Vec::new());
        crate::android::set_cursor(&ctx, Id::unique("words"), (0, 19));
        for events in [
            vec![key(Key::C)],
            vec![key(Key::X)],
            vec![Event::Copy],
            vec![Event::Cut],
        ] {
            assert!(!frame(&mut words, events), "nothing is copied");
            assert_eq!(words, "apple banana cherry", "nothing is cut");
        }
        // The same field without `private` does copy, so the check above can see one.
        let mut text = "hello".to_owned();
        let mut copied = false;
        for events in [Vec::new(), Vec::new(), vec![Event::Copy]] {
            if events.is_empty() {
                crate::android::set_cursor(&ctx, Id::unique("normal"), (0, 5));
            }
            let raw = RawInput {
                events,
                ..RawInput::default()
            };
            let mut output = ctx.run_ui(raw, |ui| {
                let field = ui.add(ui::field(&mut text).id(Id::unique("normal")));
                field.request_focus();
            });
            output.textures_delta.clear();
            copied |= output
                .platform_output
                .commands
                .iter()
                .any(|command| matches!(command, OutputCommand::CopyText(_)));
        }
        assert!(copied);
    }
}
