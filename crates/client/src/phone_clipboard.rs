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

/// A picked action, run at the start of the next frame for `field`.
static PENDING: Mutex<Option<(Id, Action)>> = Mutex::new(None);

fn menu_open_id() -> Id {
    Id::unique("phone clipboard menu open")
}

/// The long-press menu of a text field (a long press is egui's secondary click).
/// Copy and Cut only with a selection, and never out of a `secret` field.
pub fn menu(output: &TextEditOutput, secret: bool) {
    let response = &output.response.response;
    let ctx = &response.ctx;
    // Pressing a field moves its cursor, so a long press keeps the selection from
    // before the press, to copy or cut it.
    let before = response.id.with("selection before press");
    let mut state = TextEditState::load(ctx, response.id);
    let range = state.as_ref().and_then(|state| state.cursor.char_range());
    if response.long_touched() {
        if let Some(state) = &mut state
            && let Some(kept) = ctx.data(|data| data.get_temp::<CCursorRange>(before))
            && !kept.is_empty()
        {
            state.cursor.set_char_range(Some(kept));
            state.clone().store(ctx, response.id);
        }
    } else if !response.is_pointer_button_down_on()
        && let Some(range) = range
    {
        ctx.data_mut(|data| data.insert_temp(before, range));
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
    let picked = ui::context_menu(response, |m| {
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
        *lock() = Some((response.id, action));
        ctx.request_repaint();
    }
}

fn lock() -> std::sync::MutexGuard<'static, Option<(Id, Action)>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Paste the system clipboard into the focused field (a hardware keyboard's paste).
pub fn paste_focused(ctx: &Context) {
    if let Some(field) = ctx.memory(|memory| memory.focused()) {
        system::read(ctx);
        *lock() = Some((field, Action::Paste));
    }
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
    ctx.memory_mut(|memory| memory.request_focus(field));
    raw_input.events.push(event);
}

/// At the end of the frame: text the app copied goes to the system clipboard.
pub fn frame_end(ctx: &Context) {
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
        system::write(text);
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

    pub fn write(text: Zeroizing<String>) {
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
    use objc2_foundation::NSString;
    use objc2_ui_kit::UIPasteboard;
    use zeroize::Zeroizing;

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

    pub fn write(text: Zeroizing<String>) {
        let text = NSString::from_str(&text);
        // SAFETY: as in `take`.
        unsafe { UIPasteboard::generalPasteboard().setString(Some(&text)) };
    }
}

/// Tests: a clipboard inside the process.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod system {
    use std::sync::Mutex;

    use eframe::egui::Context;
    use zeroize::Zeroizing;

    pub static CLIPBOARD: Mutex<Option<String>> = Mutex::new(None);

    pub fn read(_: &Context) {}

    pub fn take() -> Option<Option<Zeroizing<String>>> {
        Some(CLIPBOARD.lock().unwrap().clone().map(Zeroizing::new))
    }

    pub fn write(text: Zeroizing<String>) {
        *CLIPBOARD.lock().unwrap() = Some(text.to_string());
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

        *lock() = Some((field, Action::SelectAll));
        frame(&ctx, &mut text);
        *lock() = Some((field, Action::Copy));
        frame(&ctx, &mut text);
        assert_eq!(
            system::CLIPBOARD.lock().unwrap().as_deref(),
            Some("hello world")
        );

        *system::CLIPBOARD.lock().unwrap() = Some("pasted".to_owned());
        *lock() = Some((field, Action::Paste));
        frame(&ctx, &mut text);
        assert_eq!(text, "pasted");
        assert!(lock().is_none());
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
}
