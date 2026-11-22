//! What remote control may do on macOS: key codes, the Ctrl/Cmd swap, the blocked
//! shortcuts and where input may land. Plain logic, so it is tested on every system;
//! `input_macos.rs` reads the desktop and posts the events.
//!
//! Every rule fails closed: an unknown window, focus, session or layout blocks input.

/// A rectangle in global display points with the origin at the top left of the main
/// display, the space `CGEvent` positions and the Accessibility API use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Frame {
    pub fn contains(self, (x, y): (f64, f64)) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn center(self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    fn near(self, other: Frame) -> bool {
        let close = |a: f64, b: f64| (a - b).abs() <= 2.0;
        close(self.x, other.x)
            && close(self.y, other.y)
            && close(self.width, other.width)
            && close(self.height, other.height)
    }
}

/// A point on the source (0 to 1 each way) as a point inside `frame`, kept a point off
/// its far edges so it never lands on the neighbouring display or window.
pub fn place(frame: Frame, (x, y): (f32, f32)) -> Option<(f64, f64)> {
    if !(frame.width >= 1.0 && frame.height >= 1.0) {
        return None;
    }
    let along = |from: f64, size: f64, at: f32| {
        from + (size * f64::from(at.clamp(0.0, 1.0))).min(size - 1.0)
    };
    Some((
        along(frame.x, frame.width, x),
        along(frame.y, frame.height, y),
    ))
}

/// An on-screen window, as the window list reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub id: u32,
    pub pid: i32,
    /// The owning process's name.
    pub owner: String,
    /// Whether the owner is an ordinary app with a Dock icon (activation policy
    /// "regular"). The Dock, Control Center, the menu bar and other system UI aren't.
    pub regular: bool,
    /// 0 for normal app windows; menus, the Dock and the menu bar sit above.
    pub layer: i64,
    pub alpha: f64,
    pub frame: Frame,
}

/// System processes that never receive remote input, whatever they report.
const SYSTEM: &[&str] = &[
    "Window Server",
    "WindowServer",
    "Dock",
    "SystemUIServer",
    "Control Center",
    "ControlCenter",
    "Notification Center",
    "NotificationCenter",
    "loginwindow",
    "SecurityAgent",
    "coreauthd",
    "Spotlight",
    "screencaptureui",
];

impl Window {
    /// A normal window of an ordinary app that isn't RustTalk.
    fn ordinary(&self, own: i32) -> bool {
        self.layer == 0 && self.regular && self.pid != own && !SYSTEM.contains(&&*self.owner)
    }
}

/// The share as it is on the desktop now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shared {
    Display(Frame),
    Window(u32),
}

/// The top of every display, where the menu bar is or slides in.
pub const MENU_BAR: f64 = 40.0;
/// Display edges, where a hidden Dock or menu bar slides in.
pub const EDGE: f64 = 4.0;
/// Display corners, where hot corners act.
pub const CORNER: f64 = 24.0;

/// Whether a point is on a display, away from its menu bar strip, edges and corners.
pub fn clear_of_system_zones(displays: &[Frame], (x, y): (f64, f64)) -> bool {
    let Some(display) = displays.iter().find(|display| display.contains((x, y))) else {
        return false;
    };
    let (left, top) = (x - display.x, y - display.y);
    let (right, bottom) = (display.width - left, display.height - top);
    let corner = (left < CORNER || right <= CORNER) && (top < CORNER || bottom <= CORNER);
    top >= MENU_BAR && left >= EDGE && right > EDGE && bottom > EDGE && !corner
}

/// Whether a pointer event at `point` reaches the share. The topmost visible window
/// there must be the shared window, or for a display, an ordinary app window on it;
/// the bare desktop, system UI and RustTalk never get remote clicks. `windows` runs
/// front to back, and `None` (the list couldn't be read) blocks.
pub fn hits(
    windows: Option<&[Window]>,
    displays: &[Frame],
    point: (f64, f64),
    shared: Shared,
    own: i32,
) -> bool {
    let Some(windows) = windows else {
        return false;
    };
    if !clear_of_system_zones(displays, point) {
        return false;
    }
    let Some(top) = windows
        .iter()
        .find(|window| window.alpha > 0.0 && window.frame.contains(point))
    else {
        return false;
    };
    top.ordinary(own)
        && match shared {
            Shared::Window(id) => top.id == id,
            Shared::Display(frame) => frame.contains(point),
        }
}

/// The keyboard focus, as the Accessibility API reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Focus {
    /// The focused app.
    pub pid: i32,
    /// Its focused window's window ID, when macOS tells.
    pub window_id: Option<u32>,
    /// Its focused window's role and frame.
    pub window_role: Option<String>,
    pub window_frame: Option<Frame>,
    /// The focused element's role, if the app reports one.
    pub element_role: Option<String>,
    /// The focused element is the focused window or inside it (true when the app
    /// reports no focused element, so keys go to the window).
    pub element_in_window: bool,
}

/// Popup and context menus, which take keys while open.
const POPUP_MENU_LAYER: i64 = 101;
const MENU_ROLES: &[&str] = &["AXMenu", "AXMenuBar", "AXMenuBarItem", "AXMenuItem"];

/// Whether keys would go to the share: the focused app is an ordinary app that isn't
/// RustTalk, no menu is open, the focused element is in its focused window, and that
/// window is the shared window or sits on the shared display. Unknown focus blocks.
pub fn focused(
    windows: Option<&[Window]>,
    focus: Option<&Focus>,
    shared: Shared,
    own: i32,
) -> bool {
    let (Some(windows), Some(focus)) = (windows, focus) else {
        return false;
    };
    let Some(app) = windows.iter().find(|window| window.pid == focus.pid) else {
        return false;
    };
    let menu_open = windows
        .iter()
        .any(|window| window.layer == POPUP_MENU_LAYER && window.alpha > 0.0);
    let in_menu = focus
        .element_role
        .as_deref()
        .is_some_and(|role| MENU_ROLES.contains(&role));
    if focus.pid == own
        || !app.regular
        || SYSTEM.contains(&&*app.owner)
        || menu_open
        || in_menu
        || !focus.element_in_window
        || focus.window_role.as_deref() != Some("AXWindow")
    {
        return false;
    }
    let Some(frame) = focus.window_frame else {
        return false;
    };
    match shared {
        Shared::Window(id) => match focus.window_id {
            Some(focused) => {
                focused == id && windows.iter().any(|w| w.id == id && w.pid == focus.pid)
            }
            // Without the ID, the frame must point at exactly one window of the app:
            // two windows with the same frame are ambiguous.
            None => {
                let mut same = windows.iter().filter(|window| {
                    window.pid == focus.pid && window.layer == 0 && window.frame.near(frame)
                });
                same.next().is_some_and(|window| window.id == id) && same.next().is_none()
            }
        },
        Shared::Display(display) => display.contains(frame.center()),
    }
}

/// The login session: on the console and unlocked, or input is refused. A missing
/// answer counts as locked.
pub fn session_open(on_console: Option<bool>, locked: Option<bool>) -> bool {
    on_console == Some(true) && locked != Some(true)
}

// macOS virtual key codes (kVK_*) that the rules below name.
const TAB: u16 = 0x30;
const SPACE: u16 = 0x31;
const ESCAPE: u16 = 0x35;
const Q: u16 = 0x0C;
const H: u16 = 0x04;
const D: u16 = 0x02;
const W: u16 = 0x0D;
const M: u16 = 0x2E;
const BACKTICK: u16 = 0x32;
const ISO_SECTION: u16 = 0x0A;
const DIGIT_3: u16 = 0x14;
const DIGIT_4: u16 = 0x15;
const DIGIT_5: u16 = 0x17;
const DIGIT_6: u16 = 0x16;
const F_KEYS: [u16; 12] = [
    0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
];
const COMMAND_LEFT: u16 = 0x37;
const COMMAND_RIGHT: u16 = 0x36;
const SHIFT_LEFT: u16 = 0x38;
const SHIFT_RIGHT: u16 = 0x3C;
const OPTION_LEFT: u16 = 0x3A;
const OPTION_RIGHT: u16 = 0x3D;

/// Protocol keys (US positions, see `control::KEYS`) as macOS virtual key codes.
///
/// Ctrl becomes Command: a viewer on Windows or Linux presses Ctrl+C, Ctrl+V or
/// Ctrl+Z and expects copy, paste and undo, which are Command shortcuts on a Mac. A
/// viewer on a Mac sends its Command key as Ctrl (see `call/remote.rs`), so it
/// arrives as Command again. Alt is Option. The Mac's own Control and Fn keys are
/// never injected, like the Windows key elsewhere; right-click stands in for
/// Control-click. The PC Menu key has no Mac counterpart and is dropped. The ISO key
/// beside left Shift is kVK_ISO_Section, as winit reads it on a Mac.
const MAC_KEYS: &[(&str, u16)] = &[
    ("A", 0x00),
    ("S", 0x01),
    ("D", D),
    ("F", 0x03),
    ("H", H),
    ("G", 0x05),
    ("Z", 0x06),
    ("X", 0x07),
    ("C", 0x08),
    ("V", 0x09),
    ("IntlBackslash", ISO_SECTION),
    ("B", 0x0B),
    ("Q", Q),
    ("W", W),
    ("E", 0x0E),
    ("R", 0x0F),
    ("Y", 0x10),
    ("T", 0x11),
    ("1", 0x12),
    ("2", 0x13),
    ("3", DIGIT_3),
    ("4", DIGIT_4),
    ("6", DIGIT_6),
    ("5", DIGIT_5),
    ("Equals", 0x18),
    ("9", 0x19),
    ("7", 0x1A),
    ("Minus", 0x1B),
    ("8", 0x1C),
    ("0", 0x1D),
    ("CloseBracket", 0x1E),
    ("O", 0x1F),
    ("U", 0x20),
    ("OpenBracket", 0x21),
    ("I", 0x22),
    ("P", 0x23),
    ("Enter", 0x24),
    ("L", 0x25),
    ("J", 0x26),
    ("Quote", 0x27),
    ("K", 0x28),
    ("Semicolon", 0x29),
    ("Backslash", 0x2A),
    ("Comma", 0x2B),
    ("Slash", 0x2C),
    ("N", 0x2D),
    ("M", M),
    ("Period", 0x2F),
    ("Tab", TAB),
    ("Space", SPACE),
    ("Backtick", BACKTICK),
    ("Backspace", 0x33),
    ("Escape", ESCAPE),
    ("ControlRight", COMMAND_RIGHT),
    ("ControlLeft", COMMAND_LEFT),
    ("ShiftLeft", SHIFT_LEFT),
    ("CapsLock", 0x39),
    ("AltLeft", OPTION_LEFT),
    ("ShiftRight", SHIFT_RIGHT),
    ("AltRight", OPTION_RIGHT),
    ("F5", F_KEYS[4]),
    ("F6", F_KEYS[5]),
    ("F7", F_KEYS[6]),
    ("F3", F_KEYS[2]),
    ("F8", F_KEYS[7]),
    ("F9", F_KEYS[8]),
    ("F11", F_KEYS[10]),
    ("F10", F_KEYS[9]),
    ("F12", F_KEYS[11]),
    ("Insert", 0x72),
    ("Home", 0x73),
    ("PageUp", 0x74),
    ("Delete", 0x75),
    ("F4", F_KEYS[3]),
    ("End", 0x77),
    ("F2", F_KEYS[1]),
    ("PageDown", 0x79),
    ("F1", F_KEYS[0]),
    ("Left", 0x7B),
    ("Right", 0x7C),
    ("Down", 0x7D),
    ("Up", 0x7E),
];

/// The macOS virtual key code for a protocol key, if it may be injected here.
pub fn key_code(key: &str) -> Option<u16> {
    MAC_KEYS
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, code)| *code)
}

// CGEventFlags masks.
pub const SHIFT: u64 = 1 << 17;
pub const OPTION: u64 = 1 << 19;
pub const COMMAND: u64 = 1 << 20;

/// The flag a modifier key sets while held, or `None` for other keys.
pub fn modifier(code: u16) -> Option<u64> {
    match code {
        SHIFT_LEFT | SHIFT_RIGHT => Some(SHIFT),
        OPTION_LEFT | OPTION_RIGHT => Some(OPTION),
        COMMAND_LEFT | COMMAND_RIGHT => Some(COMMAND),
        _ => None,
    }
}

/// The US key code of a character that a blocked chord names, so the blocklist
/// also catches the key that types it in the sharer's layout.
fn us_code(character: char) -> Option<u16> {
    Some(match character.to_ascii_lowercase() {
        'q' => Q,
        'h' => H,
        'd' => D,
        'w' => W,
        'm' => M,
        '`' => BACKTICK,
        ' ' => SPACE,
        '\t' => TAB,
        '\u{1b}' => ESCAPE,
        '3' => DIGIT_3,
        '4' => DIGIT_4,
        '5' => DIGIT_5,
        '6' => DIGIT_6,
        _ => return None,
    })
}

/// A Command chord on a US key that macOS or the app acts on beyond the shared window.
fn chord(flags: u64, code: u16, window_share: bool) -> bool {
    let (shift, option) = (flags & SHIFT != 0, flags & OPTION != 0);
    match code {
        TAB | SPACE | Q | H => true,
        ESCAPE | D | W | M => option,
        DIGIT_3 | DIGIT_4 | DIGIT_5 | DIGIT_6 => shift,
        BACKTICK | ISO_SECTION => window_share,
        _ => false,
    }
}

/// Keys a controller never presses, with these modifiers held. `layout` gives the
/// character the key types with Command in the sharer's keyboard layout; it is only
/// asked for Command chords, and a chord whose character is unknown is blocked.
///
/// - F1 to F12, with or without modifiers: Mission Control, Show Desktop, brightness,
///   VoiceOver (Command+F5) and other system actions.
/// - Command with Tab (app switcher), Space (Spotlight, input sources), Q (quits the
///   whole app; with Shift logs out, with Control locks) or H (hides the app; with
///   Option hides every other app).
/// - Command+Option with Escape (Force Quit), D (shows or hides the Dock), W
///   (closes every window of the app) or M (minimizes every window of the app).
///   Command+W and Command+M alone act on the shared window.
/// - On a window share, Command with ` (or the ISO key beside it), with or without
///   Shift: it moves the focus to the app's other windows.
/// - Command+Shift with 3, 4, 5 or 6: screenshots and recordings saved on the Mac.
///
/// A chord is checked both by the key's US position and by the character it types,
/// so Command plus the AZERTY "A" key (which types Q) is blocked too.
pub fn blocked(
    flags: u64,
    code: u16,
    window_share: bool,
    layout: impl FnOnce(u16) -> Option<char>,
) -> bool {
    if F_KEYS.contains(&code) {
        return true;
    }
    if flags & COMMAND == 0 || modifier(code).is_some() {
        return false;
    }
    if chord(flags, code, window_share) {
        return true;
    }
    match layout(code) {
        Some(character) => us_code(character).is_some_and(|us| chord(flags, us, window_share)),
        None => true,
    }
}

/// Whether a key is Command.
fn command(code: u16) -> bool {
    matches!(code, COMMAND_LEFT | COMMAND_RIGHT)
}

/// Holds back Command presses until another key, a click or a scroll needs them, so a
/// bare tap of Command (twice opens Siri) never reaches macOS. The modifiers on every
/// event already say Command is held.
#[derive(Debug, Default)]
pub struct DeferredCommand {
    pending: std::collections::BTreeSet<u16>,
    posted: std::collections::BTreeSet<u16>,
}

impl DeferredCommand {
    /// A key goes down: whether to post it now. A Command press waits, and so do
    /// its repeats until it was posted.
    pub fn down(&mut self, code: u16) -> bool {
        if !command(code) || self.posted.contains(&code) {
            return true;
        }
        self.pending.insert(code);
        false
    }

    /// Before another key, a click or a scroll: the Command presses to post first.
    pub fn flush(&mut self) -> Vec<u16> {
        let pending: Vec<u16> = std::mem::take(&mut self.pending).into_iter().collect();
        self.posted.extend(&pending);
        pending
    }

    /// A key goes up: whether to post it. A Command press never posted isn't.
    pub fn up(&mut self, code: u16) -> bool {
        self.posted.remove(&code);
        !self.pending.remove(&code)
    }
}

/// What remote input holds, whether control is on, and whether a check failed, behind
/// one lock: failing, cleaning up and letting an event through can't interleave, so
/// nothing is pressed after the cleanup that ends control.
#[derive(Debug, Default)]
pub struct Gate<H> {
    pub held: H,
    pub active: bool,
    pub failed: bool,
}

impl<H: Default> Gate<H> {
    /// Let go of everything with `release`, forget it, and refuse input until control
    /// ends. True the first time.
    pub fn fail(&mut self, release: impl FnOnce(&mut H)) -> bool {
        release(&mut self.held);
        self.held = H::default();
        !std::mem::replace(&mut self.failed, true)
    }

    /// Control starts or ends, with nothing held. A failure is forgotten only once
    /// control ended.
    pub fn set_active(&mut self, active: bool, release: impl FnOnce(&mut H)) {
        release(&mut self.held);
        self.held = H::default();
        self.active = active;
        if !active {
            self.failed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::KEYS;

    #[test]
    fn every_key_but_menu_maps_once() {
        for (name, _) in KEYS {
            assert_eq!(key_code(name).is_none(), *name == "Menu", "{name}");
        }
        assert_eq!(MAC_KEYS.len(), KEYS.len() - 1);
        let mut codes: Vec<_> = MAC_KEYS.iter().map(|(_, code)| *code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), MAC_KEYS.len(), "two keys share a code");
        // Names outside the protocol, such as the Mac's own Control or Command, never map.
        for name in ["SuperLeft", "MetaLeft", "Fn", "PrintScreen", "a"] {
            assert_eq!(key_code(name), None);
        }
    }

    #[test]
    fn ctrl_is_command_and_alt_is_option() {
        assert_eq!(key_code("A"), Some(0x00));
        assert_eq!(key_code("C"), Some(0x08));
        assert_eq!(key_code("Enter"), Some(0x24));
        assert_eq!(key_code("Backspace"), Some(0x33));
        assert_eq!(key_code("Delete"), Some(0x75));
        assert_eq!(key_code("F5"), Some(0x60));
        assert_eq!(key_code("ControlLeft").and_then(modifier), Some(COMMAND));
        assert_eq!(key_code("ControlRight").and_then(modifier), Some(COMMAND));
        assert_eq!(key_code("AltLeft").and_then(modifier), Some(OPTION));
        assert_eq!(key_code("ShiftRight").and_then(modifier), Some(SHIFT));
        assert_eq!(key_code("CapsLock").and_then(modifier), None);
        assert_eq!(key_code("Q").and_then(modifier), None);
    }

    fn code(key: &str) -> u16 {
        key_code(key).unwrap()
    }

    /// A US layout: each key types the letter or digit printed on it.
    fn us(code: u16) -> Option<char> {
        MAC_KEYS
            .iter()
            .find(|(_, c)| *c == code)
            .and_then(|(name, _)| match *name {
                "Space" => Some(' '),
                "Tab" => Some('\t'),
                "Escape" => Some('\u{1b}'),
                name if name.len() == 1 => name.chars().next().map(|c| c.to_ascii_lowercase()),
                _ => Some('?'),
            })
    }

    /// French AZERTY: A and Q, Z and W swap places, digits need Shift.
    fn azerty(code: u16) -> Option<char> {
        Some(match code {
            0x00 => 'q',
            0x0C => 'a',
            0x06 => 'w',
            0x0D => 'z',
            0x14 => '"',
            0x15 => '\'',
            _ => return us(code),
        })
    }

    #[test]
    fn system_shortcuts_are_blocked() {
        let (cmd, cmd_shift, cmd_opt) = (COMMAND, COMMAND | SHIFT, COMMAND | OPTION);
        for (flags, key) in [
            (cmd, "Tab"),
            (cmd_shift, "Tab"),
            (cmd, "Space"),
            (cmd_opt, "Space"),
            (cmd, "Q"),
            (cmd_shift, "Q"),
            (cmd, "H"),
            (cmd_opt, "H"),
            (cmd_opt, "Escape"),
            (cmd_opt | SHIFT, "Escape"),
            (cmd_opt, "D"),
            (cmd_opt, "W"),
            (cmd_shift, "3"),
            (cmd_shift, "4"),
            (cmd_shift, "5"),
            (cmd_shift, "6"),
            (cmd, "F5"),
            (cmd_opt, "F5"),
            (0, "F1"),
            (0, "F3"),
            (0, "F11"),
            (SHIFT, "F12"),
        ] {
            assert!(blocked(flags, code(key), false, us), "{flags:#x} {key}");
        }
    }

    #[test]
    fn app_shortcuts_go_through() {
        for (flags, key) in [
            (COMMAND, "C"),
            (COMMAND, "V"),
            (COMMAND, "X"),
            (COMMAND, "Z"),
            (COMMAND | SHIFT, "Z"),
            (COMMAND, "A"),
            (COMMAND, "S"),
            (COMMAND, "W"),
            (COMMAND, "D"),
            (COMMAND, "3"),
            (COMMAND, "Escape"),
            (COMMAND, "ShiftLeft"),
            (OPTION, "Tab"),
            (OPTION, "Space"),
            (SHIFT, "Q"),
            (0, "Q"),
            (0, "Tab"),
            (0, "Escape"),
            (SHIFT | OPTION, "3"),
            (COMMAND, "M"),
        ] {
            assert!(!blocked(flags, code(key), false, us), "{flags:#x} {key}");
            assert!(!blocked(flags, code(key), true, us), "{flags:#x} {key}");
        }
    }

    #[test]
    fn minimize_all_and_window_cycling_are_blocked() {
        let cmd_opt = COMMAND | OPTION;
        assert!(blocked(cmd_opt, code("M"), false, us));
        // AZERTY types M on the US Semicolon key.
        let azerty_m = |code| (code == 0x29).then_some('m').or_else(|| us(code));
        assert!(blocked(cmd_opt, code("Semicolon"), false, azerty_m));
        assert!(!blocked(cmd_opt, code("Semicolon"), false, us));
        // Command+` cycles the app's windows: blocked on window shares only.
        for flags in [COMMAND, COMMAND | SHIFT] {
            assert!(blocked(flags, code("Backtick"), true, us));
            assert!(blocked(flags, code("IntlBackslash"), true, us));
            assert!(!blocked(flags, code("Backtick"), false, us));
        }
        // A layout that types ` on another key.
        let moved = |code| (code == 0x27).then_some('`').or_else(|| us(code));
        assert!(blocked(COMMAND, code("Quote"), true, moved));
        assert!(!blocked(0, code("Backtick"), true, us));
    }

    #[test]
    fn a_bare_command_tap_never_reaches_macos() {
        let mut command = DeferredCommand::default();
        let cmd = code("ControlLeft");
        // Two quick taps of Command alone: nothing is posted.
        for _ in 0..2 {
            assert!(!command.down(cmd));
            assert!(!command.up(cmd));
        }
        // Command+C: Command goes out just before C, and its release after.
        assert!(!command.down(cmd));
        assert_eq!(command.flush(), vec![cmd]);
        assert!(command.down(code("C")));
        assert!(command.up(code("C")));
        assert!(command.up(cmd));
        // Other modifiers aren't held back. A repeat waits like the press, until
        // the press was posted.
        assert!(command.down(code("ShiftLeft")));
        assert!(!command.down(cmd));
        assert!(!command.down(cmd));
        assert_eq!(command.flush(), vec![cmd]);
        assert!(command.down(cmd));
        assert!(command.flush().is_empty());
        assert!(command.up(cmd));
    }

    #[test]
    fn nothing_is_pressed_after_the_cleanup_that_ends_control() {
        use std::sync::{Arc, Mutex, mpsc};
        let gate = Arc::new(Mutex::new(Gate::<Vec<&str>>::default()));
        gate.lock().unwrap().set_active(true, |_| {});
        gate.lock().unwrap().held.push("held key");
        let (in_cleanup, cleaning) = mpsc::channel();
        let (go_on, waiting) = mpsc::channel::<()>();
        let failing = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                let mut state = gate.lock().unwrap();
                state.fail(|held| {
                    assert_eq!(held, &vec!["held key"]);
                    in_cleanup.send(()).unwrap();
                    // Hold the lock until the event below has started.
                    waiting.recv().unwrap();
                })
            })
        };
        cleaning.recv().unwrap();
        // An event arrives during the cleanup: it waits for the lock, then must see
        // the failure and press nothing.
        let event = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                let mut state = gate.lock().unwrap();
                if state.failed {
                    return false;
                }
                state.held.push("pressed");
                true
            })
        };
        go_on.send(()).unwrap();
        assert!(failing.join().unwrap(), "first failure");
        assert!(
            !event.join().unwrap(),
            "an event got through after the cleanup"
        );
        let mut state = gate.lock().unwrap();
        assert!(state.held.is_empty());
        // A second failure isn't reported again; ending control clears it.
        assert!(!state.fail(|_| {}));
        state.set_active(false, |_| {});
        assert!(!state.failed && !state.active);
    }

    #[test]
    fn the_layout_decides_which_key_is_a_shortcut() {
        // On AZERTY the key in the US "A" place types Q: Command+it quits.
        assert!(blocked(COMMAND, code("A"), false, azerty));
        assert!(!blocked(COMMAND, code("A"), false, us));
        // The key in the US "Z" place types W: Command+Option+it closes every window.
        assert!(blocked(COMMAND | OPTION, code("Z"), false, azerty));
        // The US position still counts, whatever the layout types there.
        assert!(blocked(COMMAND, code("Q"), false, azerty));
        // Plain keys never ask the layout.
        assert!(!blocked(0, code("A"), false, |_| panic!(
            "asked the layout"
        )));
        // An unknown layout blocks Command chords only.
        assert!(blocked(COMMAND, code("C"), false, |_| None));
        assert!(!blocked(SHIFT, code("C"), false, |_| None));
    }

    const DISPLAY: Frame = Frame {
        x: -1440.0,
        y: 0.0,
        width: 1440.0,
        height: 900.0,
    };
    const MAIN_DISPLAY: Frame = Frame {
        x: 0.0,
        y: 0.0,
        width: 1920.0,
        height: 1080.0,
    };
    const DISPLAYS: [Frame; 2] = [DISPLAY, MAIN_DISPLAY];

    #[test]
    fn points_stay_on_the_share() {
        assert_eq!(place(DISPLAY, (0.0, 0.0)), Some((-1440.0, 0.0)));
        assert_eq!(place(DISPLAY, (0.5, 0.5)), Some((-720.0, 450.0)));
        // The far edges stay a point inside, never on the next display.
        assert_eq!(place(DISPLAY, (1.0, 1.0)), Some((-1.0, 899.0)));
        assert!(DISPLAY.contains(place(DISPLAY, (1.0, 1.0)).unwrap()));
        let window = Frame {
            x: 100.5,
            y: 40.0,
            width: 800.0,
            height: 600.0,
        };
        assert_eq!(place(window, (0.25, 0.5)), Some((300.5, 340.0)));
        // A window with no size has nowhere to click.
        let empty = Frame {
            width: 0.0,
            ..window
        };
        assert_eq!(place(empty, (0.5, 0.5)), None);
        let nan = Frame {
            height: f64::NAN,
            ..window
        };
        assert_eq!(place(nan, (0.5, 0.5)), None);
    }

    #[test]
    fn menu_bar_edges_and_corners_are_off_limits() {
        let clear = |point| clear_of_system_zones(&DISPLAYS, point);
        assert!(clear((-720.0, 450.0)));
        assert!(clear((960.0, MENU_BAR)));
        // The menu bar strip of either display.
        assert!(!clear((960.0, 10.0)));
        assert!(!clear((-720.0, MENU_BAR - 1.0)));
        // Edges, where a hidden Dock slides in, and corners, where hot corners act.
        assert!(!clear((960.0, 1079.0)));
        assert!(!clear((1.0, 500.0)));
        assert!(!clear((1917.0, 500.0)));
        assert!(!clear((-1.0, 500.0)));
        assert!(!clear((10.0, 1070.0)));
        assert!(!clear((1900.0, 1060.0)));
        assert!(clear((30.0, 1050.0)));
        // Off every display.
        assert!(!clear((5000.0, 500.0)));
        assert!(!clear_of_system_zones(&[], (960.0, 500.0)));
    }

    fn window(id: u32, pid: i32, layer: i64, frame: Frame) -> Window {
        Window {
            id,
            pid,
            owner: format!("App {pid}"),
            regular: true,
            layer,
            alpha: 1.0,
            frame,
        }
    }

    const OURS: i32 = 7;
    const LEFT: Frame = Frame {
        x: -1400.0,
        y: 100.0,
        width: 600.0,
        height: 400.0,
    };
    const MAIN: Frame = Frame {
        x: 100.0,
        y: 100.0,
        width: 600.0,
        height: 400.0,
    };

    #[test]
    fn pointer_lands_only_on_the_share() {
        let shared_window = window(10, 2, 0, LEFT);
        let rusttalk = window(11, OURS, 0, MAIN);
        let invisible = Window {
            alpha: 0.0,
            ..window(12, 3, 25, DISPLAY)
        };
        let windows = [invisible, rusttalk, shared_window.clone()];
        let hit =
            |windows: &[Window], point, shared| hits(Some(windows), &DISPLAYS, point, shared, OURS);
        let on_left = (-1200.0, 200.0);
        assert!(hit(&windows, on_left, Shared::Window(10)));
        assert!(hit(&windows, on_left, Shared::Display(DISPLAY)));
        // Another window on top of the shared one takes the click: dropped.
        let menu = window(13, 4, 101, LEFT);
        let covered = [menu, shared_window.clone()];
        assert!(!hit(&covered, on_left, Shared::Window(10)));
        // On a display share, system UI on top blocks too.
        assert!(!hit(&covered, on_left, Shared::Display(DISPLAY)));
        let dock = Window {
            owner: "Dock".into(),
            ..window(14, 5, 0, LEFT)
        };
        assert!(!hit(
            &[dock, shared_window.clone()],
            on_left,
            Shared::Display(DISPLAY)
        ));
        let accessory = Window {
            regular: false,
            ..window(15, 6, 0, LEFT)
        };
        assert!(!hit(&[accessory], on_left, Shared::Display(DISPLAY)));
        // RustTalk's own windows never get remote clicks.
        let over_ours = [window(11, OURS, 0, LEFT), shared_window.clone()];
        assert!(!hit(&over_ours, on_left, Shared::Display(DISPLAY)));
        // The bare desktop, a point off the share, and an unreadable window list.
        assert!(!hit(&[], on_left, Shared::Display(DISPLAY)));
        assert!(!hit(
            &[window(16, 2, 0, MAIN)],
            (300.0, 300.0),
            Shared::Display(DISPLAY)
        ));
        assert!(!hits(None, &DISPLAYS, on_left, Shared::Window(10), OURS));
        // The menu bar strip above the shared window.
        let tall = window(10, 2, 0, Frame { y: 0.0, ..LEFT });
        assert!(!hit(&[tall], (-1200.0, 10.0), Shared::Window(10)));
    }

    fn focus(pid: i32, frame: Frame) -> Focus {
        Focus {
            pid,
            window_id: None,
            window_role: Some("AXWindow".into()),
            window_frame: Some(frame),
            element_role: Some("AXTextArea".into()),
            element_in_window: true,
        }
    }

    #[test]
    fn keys_need_the_share_focused() {
        let shared_window = window(10, 2, 0, LEFT);
        let other = window(20, 3, 0, MAIN);
        let menu_bar = Window {
            owner: "Window Server".into(),
            regular: false,
            ..window(30, 0, 24, DISPLAY)
        };
        let windows = [menu_bar, shared_window.clone(), other];
        let keys = |windows: &[Window], focus: &Focus, shared| {
            focused(Some(windows), Some(focus), shared, OURS)
        };
        let on_shared = focus(2, LEFT);
        assert!(keys(&windows, &on_shared, Shared::Window(10)));
        assert!(keys(&windows, &on_shared, Shared::Display(DISPLAY)));
        // Another app focused, on another display.
        let on_other = focus(3, MAIN);
        assert!(!keys(&windows, &on_other, Shared::Window(10)));
        assert!(!keys(&windows, &on_other, Shared::Display(DISPLAY)));
        // The shared app, but another of its windows (a panel) has the focus.
        let panel = focus(2, Frame { x: -900.0, ..LEFT });
        assert!(!keys(&windows, &panel, Shared::Window(10)));
        let floating = Focus {
            window_role: Some("AXSheet".into()),
            ..on_shared.clone()
        };
        assert!(!keys(&windows, &floating, Shared::Display(DISPLAY)));
        // The focused element sits outside the focused window.
        let outside = Focus {
            element_in_window: false,
            ..on_shared.clone()
        };
        assert!(!keys(&windows, &outside, Shared::Window(10)));
        // A menu is open or focused.
        let in_menu = Focus {
            element_role: Some("AXMenuItem".into()),
            ..on_shared.clone()
        };
        assert!(!keys(&windows, &in_menu, Shared::Window(10)));
        let popup = window(40, 2, 101, MAIN);
        let with_popup = [popup, shared_window.clone()];
        assert!(!keys(&with_popup, &on_shared, Shared::Window(10)));
        // RustTalk, system UI, an app without windows, unknown focus or windows.
        let ours = [window(1, OURS, 0, LEFT)];
        assert!(!keys(&ours, &focus(OURS, LEFT), Shared::Display(DISPLAY)));
        let dock = [Window {
            owner: "Dock".into(),
            ..window(50, 9, 0, LEFT)
        }];
        assert!(!keys(&dock, &focus(9, LEFT), Shared::Display(DISPLAY)));
        assert!(!keys(&windows, &focus(8, LEFT), Shared::Display(DISPLAY)));
        let no_window = Focus {
            window_frame: None,
            ..on_shared.clone()
        };
        assert!(!keys(&windows, &no_window, Shared::Display(DISPLAY)));
        assert!(!focused(
            Some(&windows),
            None,
            Shared::Display(DISPLAY),
            OURS
        ));
        assert!(!focused(None, Some(&on_shared), Shared::Window(10), OURS));
    }

    #[test]
    fn keys_follow_the_shared_window_not_its_twin() {
        // Two windows of one app with the same frame, e.g. after Command+`.
        let shared_window = window(10, 2, 0, LEFT);
        let twin = window(11, 2, 0, LEFT);
        let windows = [twin, shared_window];
        let keys = |focus: &Focus| focused(Some(&windows), Some(focus), Shared::Window(10), OURS);
        let by_id = |id| Focus {
            window_id: Some(id),
            ..focus(2, LEFT)
        };
        assert!(keys(&by_id(10)));
        assert!(!keys(&by_id(11)));
        // Without the window ID, the frame alone is ambiguous.
        assert!(!keys(&focus(2, LEFT)));
        // A unique frame still identifies the shared window.
        let alone = [window(10, 2, 0, LEFT)];
        assert!(focused(
            Some(&alone),
            Some(&focus(2, LEFT)),
            Shared::Window(10),
            OURS
        ));
        // The ID of a window of another app never counts.
        let other = Focus {
            window_id: Some(10),
            ..focus(3, LEFT)
        };
        assert!(!keys(&other));
    }

    #[test]
    fn a_locked_or_switched_session_blocks() {
        assert!(session_open(Some(true), Some(false)));
        assert!(session_open(Some(true), None));
        assert!(!session_open(Some(true), Some(true)));
        assert!(!session_open(Some(false), None));
        assert!(!session_open(None, None));
    }
}
