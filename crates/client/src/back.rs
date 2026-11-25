//! Back: Android's Back button and gesture, and a keyboard's Browser Back key.
//! One press closes what is on top: a menu or popup, then a dialog or settings
//! window (on a phone a settings page goes back to its list first), then what the
//! screen shows itself (the member list over a chat, a phone's open pane, an
//! onboarding step). At the root nothing is left, and Android moves the app to the
//! background.

use eframe::egui::{Context, Event, Id, Key, Modifiers, RawInput, Ui};

/// Whether a popup or menu was open at the end of the last frame. egui only knows
/// during a frame, and a press is handled before one.
fn popup_id() -> Id {
    Id::unique("back-popup")
}

/// Back went to the top dialog or settings window this frame (`take`).
fn modal_id() -> Id {
    Id::unique("back-modal")
}

/// Before a frame: takes the Back presses out of `raw_input` and handles one.
/// Menus, popups and dialogs close as with Escape; then `screen` goes back, and
/// says whether it had anywhere to go. `None` without a press, `Some(false)` at
/// the root.
pub fn input(
    ctx: &Context,
    raw_input: &mut RawInput,
    screen: impl FnOnce() -> bool,
) -> Option<bool> {
    let mut pressed = false;
    raw_input.events.retain(|event| match event {
        Event::Key {
            key: Key::BrowserBack,
            pressed: down,
            ..
        } => {
            pressed |= *down;
            false
        }
        _ => true,
    });
    if !pressed {
        return None;
    }
    let popup = ctx.data(|data| data.get_temp(popup_id())).unwrap_or(false);
    // The top dialog as last drawn (egui keeps it until the next frame ends).
    let modal = ctx.memory(|memory| memory.top_modal_layer().is_some());
    if !popup && !modal {
        return Some(screen());
    }
    if !popup {
        ctx.data_mut(|data| data.insert_temp(modal_id(), true));
    }
    for pressed in [true, false] {
        raw_input.events.push(Event::Key {
            key: Key::Escape,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
    }
    Some(true)
}

/// For a dialog with its own Back (a settings page on a phone): true when Back
/// reached it, taking the Escape that would close it so it can go back a page.
pub fn take(ui: &Ui) -> bool {
    let ctx = ui.ctx();
    if ctx.memory(|memory| memory.top_modal_layer()) != Some(ui.layer_id())
        || !ctx.data_mut(|data| data.remove_temp::<bool>(modal_id()).unwrap_or(false))
    {
        return false;
    }
    ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
    true
}

/// At the end of a frame: whether Back has something to close, with `screen` for
/// what the screen itself can go back from. A dialog that opened this frame counts
/// from the next one, which its fade-in always draws.
pub fn frame_end(ctx: &Context, screen: bool) -> bool {
    let popup = ctx.any_popup_open();
    ctx.data_mut(|data| {
        data.insert_temp(popup_id(), popup);
        data.remove::<bool>(modal_id());
    });
    popup || screen || ctx.memory(|memory| memory.top_modal_layer().is_some())
}

/// A Back press, as a keyboard's Browser Back key sends it.
#[cfg(any(target_os = "android", test))]
pub fn press(events: &mut Vec<Event>) {
    for pressed in [true, false] {
        events.push(Event::Key {
            key: Key::BrowserBack,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        });
    }
}
