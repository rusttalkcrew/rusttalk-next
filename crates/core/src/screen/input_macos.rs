//! Remote control input on macOS, posted as `CGEvent`s to the HID event tap.
//!
//! Input only lands on what is shared (`macos_rules.rs` holds the rules, the key map
//! and the blocked shortcuts). Before each click, scroll or key press RustTalk reads
//! the window list, the displays, the login session and, for keys, the Accessibility
//! focus; anything it can't read blocks the event. While remote input holds a key or
//! button, it checks again about every 100 ms; a failed check lets go of everything
//! and ends control. Releases always go through, and every held key and button is let
//! go when control ends or the share stops.
//!
//! Each check is a snapshot: the window under the pointer or the focus can change in
//! the moment between the check and macOS delivering the event.
//!
//! Posting events needs the Accessibility permission. Without it macOS drops them
//! silently, so control is refused until the person allows it.

use std::{
    collections::{BTreeSet, HashMap},
    ffi::c_void,
    ptr::NonNull,
    sync::{Arc, Mutex, MutexGuard, Weak, mpsc},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use dispatch2::DispatchQueue;
use objc2::{MainThreadMarker, rc::autoreleasepool};
use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFEqual, CFNumber, CFRetained, CFString, CFType,
    CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventTapLocation,
    CGEventType, CGGetActiveDisplayList, CGMouseButton, CGRectMakeWithDictionaryRepresentation,
    CGScrollEventUnit, CGSessionCopyCurrentDictionary, CGWindowListCopyWindowInfo,
    CGWindowListOption, kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer,
    kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
};

use super::{
    macos_rules::{
        self, DeferredCommand, Focus, Frame, Gate, Shared, Window, blocked, key_code, modifier,
    },
    platform::Target,
};
use crate::control::{Button, Input};

type AXError = i32;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
const AX_ERROR_NO_VALUE: AXError = -25212;
const AX_VALUE_CG_POINT: u32 = 1;
const AX_VALUE_CG_SIZE: u32 = 2;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: Option<&CFDictionary>) -> bool;
    static kAXTrustedCheckOptionPrompt: &'static CFString;
    fn AXUIElementCreateSystemWide() -> Option<NonNull<CFType>>;
    fn AXUIElementCopyAttributeValue(
        element: &CFType,
        attribute: &CFString,
        value: *mut *const CFType,
    ) -> AXError;
    fn AXUIElementGetPid(element: &CFType, pid: *mut i32) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: &CFType, seconds: f32) -> AXError;
    fn AXValueGetValue(value: &CFType, kind: u32, out: *mut c_void) -> bool;
    // Private, but stable since macOS 10.6 and used by window managers: the window ID
    // behind a window element.
    fn _AXUIElementGetWindow(element: &CFType, id: *mut u32) -> AXError;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> Option<NonNull<CFType>>;
    fn TISGetInputSourceProperty(source: &CFType, key: &CFString) -> *const CFData;
    static kTISPropertyUnicodeKeyLayoutData: &'static CFString;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const c_void,
        code: u16,
        action: u16,
        modifiers: u32,
        keyboard: u32,
        options: u32,
        dead_keys: *mut u32,
        max_length: usize,
        length: *mut usize,
        text: *mut u16,
    ) -> i32;
}

/// Whether macOS lets RustTalk post input (System Settings, Privacy & Security,
/// Accessibility).
pub fn trusted() -> bool {
    // SAFETY: takes no arguments and only reads the permission.
    unsafe { AXIsProcessTrusted() }
}

/// Ask macOS to show its Accessibility prompt, which leads to System Settings.
/// macOS shows it at most once per launch.
pub fn request_access() {
    // SAFETY: the key is a constant CFString from the framework, alive for the whole run.
    let key = unsafe { kAXTrustedCheckOptionPrompt };
    let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[CFBoolean::new(true)]);
    // SAFETY: the options are a dictionary of CFString keys and CFBoolean values, as
    // the function expects.
    unsafe { AXIsProcessTrustedWithOptions(Some(options.as_opaque())) };
}

/// Pixels a wheel notch scrolls, the same as the viewer counts per notch from a touchpad.
const PIXELS_PER_NOTCH: f32 = 40.0;
/// Clicks closer together than this, at almost the same place, count as one double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
const DOUBLE_CLICK_DISTANCE: f64 = 4.0;
/// How often held remote input is checked again.
const HELD_CHECK: Duration = Duration::from_millis(100);
/// How long a Command chord waits for the main thread to read the keyboard layout.
const LAYOUT_WAIT: Duration = Duration::from_millis(250);

/// What remote input holds down now, to send the right modifiers with every event and
/// to let go of all of it when control ends.
#[derive(Default)]
struct Held {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<Button>,
    /// Command presses not posted yet (see `DeferredCommand`).
    command: DeferredCommand,
    /// The last press, for the click count macOS needs to see a double click.
    last_click: Option<(Button, Instant, (f64, f64), i64)>,
    clicks: i64,
}

impl Held {
    fn flags(&self) -> u64 {
        self.keys
            .iter()
            .filter_map(|code| modifier(*code))
            .fold(0, |a, b| a | b)
    }

    /// Post the Command presses held back for this key, click or scroll.
    fn flush_command(&mut self) -> Result<()> {
        for code in self.command.flush() {
            key_event(code, true, self.flags())?;
        }
        Ok(())
    }

    /// Let go of every key and button. Best effort: what macOS doesn't take is
    /// forgotten anyway, so stale modifiers never come back.
    fn release(&mut self) {
        let buttons = std::mem::take(&mut self.buttons);
        for button in buttons {
            if let Err(error) = release_button(self, button) {
                tracing::warn!(%error, "could not release a remote mouse button");
            }
        }
        // Other keys first, so modifiers still match while they go up.
        let (modifiers, keys): (Vec<u16>, Vec<u16>) = std::mem::take(&mut self.keys)
            .into_iter()
            .partition(|code| modifier(*code).is_some());
        self.keys.extend(&modifiers);
        for code in keys.into_iter().chain(modifiers) {
            self.keys.remove(&code);
            if self.command.up(code)
                && let Err(error) = key_event(code, false, self.flags())
            {
                tracing::warn!(%error, "could not release a remote key");
            }
        }
    }
}

struct Inner {
    target: Target,
    /// Held input, whether someone controls the share, and whether a check failed,
    /// behind one lock, so no event slips in after the cleanup that ends control.
    gate: Mutex<Gate<Held>>,
}

/// Remote input into one share. A watcher thread rechecks held input while it lives.
pub struct Control(Arc<Inner>);

impl Control {
    pub fn new(target: Target) -> Self {
        let inner = Arc::new(Inner {
            target,
            gate: Mutex::default(),
        });
        let weak = Arc::downgrade(&inner);
        let spawned = std::thread::Builder::new()
            .name("remote-control-check".into())
            .spawn(move || watch(weak));
        if let Err(error) = spawned {
            // Without the watcher, held input can't be rechecked: refuse control.
            tracing::warn!(%error, "could not start the remote control check");
            inner.gate().failed = true;
        }
        Self(inner)
    }

    /// Whether control may be granted or go on: permitted, and no check failed.
    pub fn usable(&self) -> bool {
        trusted() && !self.0.gate().failed
    }

    /// Control starts or ends. Either way nothing stays held, and a failed check is
    /// forgotten only once control ended.
    pub fn set_active(&self, active: bool) {
        self.0.gate().set_active(active, Held::release);
    }

    /// `at` maps a point on the published picture to one on the source.
    pub fn inject(&self, input: &Input, at: impl Fn(f32, f32) -> Option<(f32, f32)>) -> Result<()> {
        // Network threads have no autorelease pool of their own.
        autoreleasepool(|_| self.0.inject(input, at))
    }
}

impl Inner {
    fn gate(&self) -> MutexGuard<'_, Gate<Held>> {
        self.gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Everything from the failure check to posting happens under the gate's lock.
    fn inject(&self, input: &Input, at: impl Fn(f32, f32) -> Option<(f32, f32)>) -> Result<()> {
        let mut gate = self.gate();
        if gate.failed {
            bail!("remote control is ending");
        }
        if !trusted() || !session_open() {
            fail(
                &mut gate,
                "Accessibility was turned off or the session was locked",
            );
            bail!("remote input is not allowed now");
        }
        let held = &mut gate.held;
        match input {
            Input::Move { x, y } => {
                let Some(point) = self.place(at(*x, *y))? else {
                    return Ok(());
                };
                let (kind, button) = match held.buttons.first() {
                    Some(Button::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
                    Some(Button::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
                    Some(Button::Middle) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
                    None => (CGEventType::MouseMoved, CGMouseButton::Left),
                };
                post(mouse(kind, point, button, held.flags())?);
            }
            Input::Button {
                button,
                down: true,
                x,
                y,
            } => {
                let Some(point) = self.place(at(*x, *y))? else {
                    return Ok(());
                };
                held.flush_command()?;
                let now = Instant::now();
                let again = held.last_click.is_some_and(|(last, at, place, _)| {
                    last == *button
                        && now.duration_since(at) < DOUBLE_CLICK
                        && (place.0 - point.0).hypot(place.1 - point.1) < DOUBLE_CLICK_DISTANCE
                });
                held.clicks = if again { held.clicks + 1 } else { 1 };
                held.last_click = Some((*button, now, point, held.clicks));
                held.buttons.insert(*button);
                let event = mouse(
                    press(*button, true),
                    point,
                    cg_button(*button),
                    held.flags(),
                )?;
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::MouseEventClickState,
                    held.clicks,
                );
                post(event);
            }
            Input::Button { button, .. } => release_button(held, *button)?,
            Input::Scroll { x, y } => {
                if !self.hits(cursor()?) {
                    return Ok(());
                }
                held.flush_command()?;
                let pixels = |notches: f32| (notches * PIXELS_PER_NOTCH).round() as i32;
                // Positive is up and left, as in the protocol.
                let event = CGEvent::new_scroll_wheel_event2(
                    None,
                    CGScrollEventUnit::Pixel,
                    2,
                    pixels(*y),
                    pixels(*x),
                    0,
                )
                .context("could not make a scroll event")?;
                CGEvent::set_flags(Some(&event), CGEventFlags(held.flags()));
                post(event);
            }
            Input::Key { key, down } => {
                let code = key_code(key).context("key not allowed on macOS")?;
                if *down {
                    let window_share = matches!(self.target, Target::Window(_));
                    if blocked(held.flags(), code, window_share, command_character)
                        || !self.focused()
                    {
                        return Ok(());
                    }
                    held.keys.insert(code);
                    if !held.command.down(code) {
                        return Ok(());
                    }
                    if modifier(code).is_none() {
                        held.flush_command()?;
                    }
                } else {
                    held.keys.remove(&code);
                    if !held.command.up(code) {
                        return Ok(());
                    }
                }
                key_event(code, *down, held.flags())?;
            }
        }
        Ok(())
    }

    /// Recheck held input: still permitted, the session open, held keys still going
    /// to the share, held buttons still over it. The slow reads happen outside the
    /// lock; the failure is marked under it, after checking control is still on.
    fn recheck(&self) {
        let (keys, buttons) = {
            let gate = self.gate();
            if !gate.active || gate.failed {
                return;
            }
            (!gate.held.keys.is_empty(), !gate.held.buttons.is_empty())
        };
        let why = if !trusted() || !session_open() {
            "Accessibility was turned off or the session was locked"
        } else if keys && !self.focused() {
            "the shared screen lost the keyboard focus"
        } else if buttons && !cursor().is_ok_and(|point| self.hits(point)) {
            "the pointer left the shared screen"
        } else {
            return;
        };
        let mut gate = self.gate();
        if gate.active {
            fail(&mut gate, why);
        }
    }

    fn shared(&self) -> Shared {
        match self.target {
            Target::Display(id) => Shared::Display(frame(CGDisplayBounds(id))),
            Target::Window(id) => Shared::Window(id),
        }
    }

    fn hits(&self, point: (f64, f64)) -> bool {
        let (windows, displays) = (windows(), displays().unwrap_or_default());
        macos_rules::hits(windows.as_deref(), &displays, point, self.shared(), own())
    }

    fn focused(&self) -> bool {
        let (windows, focus) = (windows(), focus());
        macos_rules::focused(windows.as_deref(), focus.as_ref(), self.shared(), own())
    }

    /// A point on the source as a point on the desktop, if it lands on the share.
    fn place(&self, point: Option<(f32, f32)>) -> Result<Option<(f64, f64)>> {
        let Some(point) = point else {
            return Ok(None);
        };
        let windows = windows().context("could not list windows")?;
        let displays = displays().context("could not list displays")?;
        let bounds = match self.shared() {
            Shared::Display(bounds) => bounds,
            Shared::Window(id) => windows
                .iter()
                .find(|window| window.id == id)
                .map(|window| window.frame)
                .context("shared window is gone or not on screen")?,
        };
        Ok(macos_rules::place(bounds, point).filter(|point| {
            macos_rules::hits(Some(&windows), &displays, *point, self.shared(), own())
        }))
    }
}

/// Let go of everything and drop input until control ends; the call ends it within a
/// couple of seconds, as when the share stops supporting control.
fn fail(gate: &mut Gate<Held>, why: &str) {
    if gate.fail(Held::release) {
        tracing::info!(why, "ending remote control of our screen");
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.gate().held.release();
    }
}

/// Recheck held input until the control is dropped.
fn watch(inner: Weak<Inner>) {
    loop {
        std::thread::sleep(HELD_CHECK);
        let Some(inner) = inner.upgrade() else {
            return;
        };
        autoreleasepool(|_| inner.recheck());
    }
}

fn release_button(held: &mut Held, button: Button) -> Result<()> {
    held.buttons.remove(&button);
    let clicks = held
        .last_click
        .filter(|(last, ..)| *last == button)
        .map_or(1, |(.., clicks)| clicks);
    let event = mouse(
        press(button, false),
        cursor()?,
        cg_button(button),
        held.flags(),
    )?;
    CGEvent::set_integer_value_field(Some(&event), CGEventField::MouseEventClickState, clicks);
    post(event);
    Ok(())
}

fn press(button: Button, down: bool) -> CGEventType {
    match (button, down) {
        (Button::Left, true) => CGEventType::LeftMouseDown,
        (Button::Left, false) => CGEventType::LeftMouseUp,
        (Button::Right, true) => CGEventType::RightMouseDown,
        (Button::Right, false) => CGEventType::RightMouseUp,
        (Button::Middle, true) => CGEventType::OtherMouseDown,
        (Button::Middle, false) => CGEventType::OtherMouseUp,
    }
}

fn cg_button(button: Button) -> CGMouseButton {
    match button {
        Button::Left => CGMouseButton::Left,
        Button::Right => CGMouseButton::Right,
        Button::Middle => CGMouseButton::Center,
    }
}

fn mouse(
    kind: CGEventType,
    (x, y): (f64, f64),
    button: CGMouseButton,
    flags: u64,
) -> Result<CFRetained<CGEvent>> {
    let event = CGEvent::new_mouse_event(None, kind, CGPoint { x, y }, button)
        .context("could not make a mouse event")?;
    CGEvent::set_flags(Some(&event), CGEventFlags(flags));
    Ok(event)
}

/// A key by its virtual key code, so the sharer's own keyboard layout picks the character.
/// The modifiers are set on every event, so a viewer's Command never mixes with keys the
/// sharer holds.
fn key_event(code: u16, down: bool, flags: u64) -> Result<()> {
    let event =
        CGEvent::new_keyboard_event(None, code, down).context("could not make a key event")?;
    CGEvent::set_flags(Some(&event), CGEventFlags(flags));
    post(event);
    Ok(())
}

fn post(event: CFRetained<CGEvent>) {
    CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
}

/// Where the pointer is now.
fn cursor() -> Result<(f64, f64)> {
    let event = CGEvent::new(None).context("could not read the pointer position")?;
    let point = CGEvent::location(Some(&event));
    Ok((point.x, point.y))
}

fn own() -> i32 {
    std::process::id() as i32
}

fn frame(rect: CGRect) -> Frame {
    Frame {
        x: rect.origin.x,
        y: rect.origin.y,
        width: rect.size.width,
        height: rect.size.height,
    }
}

/// The login session is on the console and unlocked.
fn session_open() -> bool {
    let Some(session) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    // SAFETY: the session dictionary has string keys.
    let session = unsafe { CFRetained::cast_unchecked::<CFDictionary<CFString, CFType>>(session) };
    let flag = |key: &'static str| {
        let value = session.get(&*CFString::from_static_str(key))?;
        match value.downcast::<CFBoolean>() {
            Ok(value) => Some(value.as_bool()),
            Err(value) => value
                .downcast::<CFNumber>()
                .ok()
                .and_then(|number| number.as_i64())
                .map(|number| number != 0),
        }
    };
    // `kCGSessionOnConsoleKey`, and the key macOS sets while the screen is locked.
    macos_rules::session_open(
        flag("kCGSSessionOnConsoleKey"),
        flag("CGSSessionScreenIsLocked"),
    )
}

/// The bounds of every active display, or `None` if they can't be read.
fn displays() -> Option<Vec<Frame>> {
    let mut ids: [CGDirectDisplayID; 32] = [0; 32];
    let mut count = 0;
    // SAFETY: `ids` holds 32 entries and `count` is a valid place to write.
    let error = unsafe { CGGetActiveDisplayList(32, ids.as_mut_ptr(), &mut count) };
    if error.0 != 0 {
        return None;
    }
    let count = (count as usize).min(ids.len());
    Some(
        ids[..count]
            .iter()
            .map(|id| frame(CGDisplayBounds(*id)))
            .collect(),
    )
}

/// Whether a process is an ordinary app with a Dock icon.
fn regular(pid: i32) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .is_some_and(|app| app.activationPolicy() == NSApplicationActivationPolicy::Regular)
}

/// On-screen windows, front to back, or `None` if the list can't be read in full.
fn windows() -> Option<Vec<Window>> {
    let options =
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let list = CGWindowListCopyWindowInfo(options, kCGNullWindowID)?;
    // SAFETY: the list holds one dictionary with string keys per window.
    let list =
        unsafe { CFRetained::cast_unchecked::<CFArray<CFDictionary<CFString, CFType>>>(list) };
    let mut apps = HashMap::new();
    let mut windows = Vec::new();
    for info in &*list {
        let get = |key: &CFString| info.get(key);
        let number = |key| get(key).and_then(|value| value.downcast::<CFNumber>().ok());
        // SAFETY (each `unsafe` below): the keys are constant CFStrings from the framework.
        let id = unsafe { number(kCGWindowNumber) }.and_then(|n| n.as_i64())?;
        let pid = unsafe { number(kCGWindowOwnerPID) }.and_then(|n| n.as_i64())?;
        let layer = unsafe { number(kCGWindowLayer) }.and_then(|n| n.as_i64())?;
        let bounds = unsafe { get(kCGWindowBounds) }
            .and_then(|value| value.downcast::<CFDictionary>().ok())?;
        let mut rect = CGRect::default();
        // SAFETY: the bounds are a CGRect dictionary, and `rect` is a valid place to write.
        if !unsafe { CGRectMakeWithDictionaryRepresentation(Some(&bounds), &mut rect) } {
            return None;
        }
        let (id, pid) = (u32::try_from(id).ok()?, i32::try_from(pid).ok()?);
        let owner = unsafe { get(kCGWindowOwnerName) }
            .and_then(|value| value.downcast::<CFString>().ok())
            .map(|name| name.to_string())
            .unwrap_or_default();
        windows.push(Window {
            id,
            pid,
            owner,
            regular: *apps.entry(pid).or_insert_with(|| regular(pid)),
            layer,
            // A window that doesn't say counts as visible.
            alpha: unsafe { number(kCGWindowAlpha) }
                .and_then(|n| n.as_f64())
                .unwrap_or(1.0),
            frame: frame(rect),
        });
    }
    Some(windows)
}

/// One Accessibility attribute of an element, if it has it.
fn attribute(element: &CFType, name: &'static str) -> Option<CFRetained<CFType>> {
    read(element, name).ok().flatten()
}

/// One Accessibility attribute: `Ok(None)` when the element doesn't have it, `Err`
/// when it couldn't be read, such as a timeout or a messaging error.
fn read(element: &CFType, name: &'static str) -> Result<Option<CFRetained<CFType>>, AXError> {
    let name = CFString::from_static_str(name);
    let mut value: *const CFType = std::ptr::null();
    // SAFETY: `element` is an AXUIElement; `value` receives an owned reference or stays null.
    let error = unsafe { AXUIElementCopyAttributeValue(element, &name, &mut value) };
    // SAFETY: a non-null value is owned by us (a Copy function).
    let value = NonNull::new(value.cast_mut()).map(|value| unsafe { CFRetained::from_raw(value) });
    match error {
        0 => Ok(value),
        AX_ERROR_NO_VALUE | AX_ERROR_ATTRIBUTE_UNSUPPORTED => Ok(None),
        error => Err(error),
    }
}

/// The window ID of a window element, so focus is matched by identity, not by frame.
fn window_id(window: &CFType) -> Option<u32> {
    let mut id = 0;
    // SAFETY: `window` is an AXUIElement and `id` a valid place to write.
    (unsafe { _AXUIElementGetWindow(window, &mut id) } == 0).then_some(id)
}

fn text(element: &CFType, name: &'static str) -> Option<String> {
    attribute(element, name)?
        .downcast::<CFString>()
        .ok()
        .map(|text| text.to_string())
}

/// A window element's frame, from its position and size.
fn element_frame(window: &CFType) -> Option<Frame> {
    let mut origin = CGPoint::default();
    let mut size = CGSize::default();
    let position = attribute(window, "AXPosition")?;
    let extent = attribute(window, "AXSize")?;
    // SAFETY: the attributes are AXValues of these kinds, written into matching structs.
    let read = unsafe {
        AXValueGetValue(&position, AX_VALUE_CG_POINT, (&raw mut origin).cast())
            && AXValueGetValue(&extent, AX_VALUE_CG_SIZE, (&raw mut size).cast())
    };
    read.then_some(Frame {
        x: origin.x,
        y: origin.y,
        width: size.width,
        height: size.height,
    })
}

/// Where keys go now, from the Accessibility API, or `None` if it can't tell.
fn focus() -> Option<Focus> {
    // SAFETY: returns an owned system-wide element.
    let system = unsafe { CFRetained::from_raw(AXUIElementCreateSystemWide()?) };
    // A hung app must not stall remote input.
    // SAFETY: `system` is an AXUIElement.
    unsafe { AXUIElementSetMessagingTimeout(&system, 0.25) };
    let app = attribute(&system, "AXFocusedApplication")?;
    let mut pid = 0;
    // SAFETY: `app` is an AXUIElement and `pid` a valid place to write.
    if unsafe { AXUIElementGetPid(&app, &mut pid) } != 0 {
        return None;
    }
    // Only "no such attribute" counts as no focused window or element; a read that
    // fails or times out leaves the focus unknown.
    let window = read(&app, "AXFocusedWindow").ok()?;
    let element = read(&app, "AXFocusedUIElement").ok()?;
    let element_role = match &element {
        Some(element) => Some(text(element, "AXRole")?),
        None => None,
    };
    let element_in_window = match (&element, &window) {
        (None, _) => true,
        (Some(element), Some(window)) => {
            CFEqual(Some(&**element), Some(&**window))
                || read(element, "AXWindow")
                    .ok()?
                    .is_some_and(|owner| CFEqual(Some(&*owner), Some(&**window)))
        }
        (Some(_), None) => false,
    };
    Some(Focus {
        pid,
        window_id: window.as_deref().and_then(window_id),
        window_role: window.as_deref().and_then(|window| text(window, "AXRole")),
        window_frame: window.as_deref().and_then(element_frame),
        element_role,
        element_in_window,
    })
}

/// The character a key types with Command in the current keyboard layout, which is
/// what macOS matches shortcuts against. Read on the main thread, where the text
/// input APIs must run; `None` if it can't be read in time.
fn command_character(code: u16) -> Option<char> {
    if MainThreadMarker::new().is_some() {
        return translate(code);
    }
    let (send, receive) = mpsc::sync_channel(1);
    DispatchQueue::main().exec_async(move || {
        let _ = send.send(translate(code));
    });
    receive.recv_timeout(LAYOUT_WAIT).ok().flatten()
}

fn translate(code: u16) -> Option<char> {
    // Command's bit in the Carbon modifier state, shifted down as UCKeyTranslate wants.
    const COMMAND: u32 = 0x0100 >> 8;
    const KEY_DOWN: u16 = 0;
    const NO_DEAD_KEYS: u32 = 1;
    // SAFETY: returns an owned input source, or null.
    let source = unsafe { CFRetained::from_raw(TISCopyCurrentKeyboardLayoutInputSource()?) };
    // SAFETY: `source` is an input source; the property is CFData it keeps alive.
    let data = unsafe { TISGetInputSourceProperty(&source, kTISPropertyUnicodeKeyLayoutData) };
    // SAFETY: a non-null result is a CFData owned by `source`, which outlives it here.
    let layout = unsafe { data.as_ref() }?.byte_ptr();
    if layout.is_null() {
        return None;
    }
    let (mut dead, mut length, mut out) = (0, 0, [0u16; 4]);
    // SAFETY: `layout` points at the layout's 'uchr' data; the out-parameters are valid
    // and `out` holds `out.len()` UTF-16 units.
    let status = unsafe {
        UCKeyTranslate(
            layout.cast(),
            code,
            KEY_DOWN,
            COMMAND,
            u32::from(LMGetKbdType()),
            NO_DEAD_KEYS,
            &mut dead,
            out.len(),
            &mut length,
            out.as_mut_ptr(),
        )
    };
    if status != 0 {
        return None;
    }
    char::decode_utf16(out[..length.min(out.len())].iter().copied())
        .next()?
        .ok()
}
