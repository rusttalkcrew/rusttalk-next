//! Remote control input on Windows, through `SendInput`.
//!
//! Input only lands on what is shared: pointer events on a shared window must hit that
//! window, and keys go through only while it (or, for a monitor, a window on it) has
//! the focus. RustTalk's own windows never receive remote input, so a controller can't
//! answer prompts or read chats here. Releases always go through, so nothing sticks.

use std::{ffi::c_void, mem::size_of};

use anyhow::{Context, Result, ensure};
use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::{
        Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute},
        Gdi::{GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow},
    },
    System::Threading::GetCurrentProcessId,
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
            MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
            MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
            MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY,
        },
        WindowsAndMessaging::{
            GA_ROOT, GetAncestor, GetCursorPos, GetForegroundWindow, GetSystemMetrics,
            GetWindowThreadProcessId, IsIconic, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WindowFromPoint,
        },
    },
};

use super::platform;
use crate::control::{Button, Input, scan_code};

/// The shared monitor or window, as plain handle values: the capture library's types
/// hold raw pointers, which can't be shared with the network tasks.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    Monitor(usize),
    Window(usize),
}

impl From<platform::Target> for Target {
    fn from(target: platform::Target) -> Self {
        match target {
            platform::Target::Monitor(monitor) => Self::Monitor(monitor.as_raw_hmonitor() as usize),
            platform::Target::Window(window) => Self::Window(window.as_raw_hwnd() as usize),
        }
    }
}

fn monitor(handle: usize) -> HMONITOR {
    HMONITOR(handle as *mut c_void)
}

fn window(handle: usize) -> HWND {
    HWND(handle as *mut c_void)
}

/// One wheel notch.
const WHEEL_DELTA: f32 = 120.0;

/// `at` maps a point on the published picture to one on the source (see `Remote::point`).
pub fn inject(
    target: Target,
    input: &Input,
    at: impl Fn(f32, f32) -> Option<(f32, f32)>,
) -> Result<()> {
    match input {
        Input::Move { x, y } => match place(target, at(*x, *y))? {
            Some(point) => send(&[mouse(point, MOUSEEVENTF_MOVE, 0)]),
            None => Ok(()),
        },
        Input::Button {
            button,
            down: true,
            x,
            y,
        } => match place(target, at(*x, *y))? {
            Some(point) => send(&[mouse(point, MOUSEEVENTF_MOVE | flag(*button, true), 0)]),
            None => Ok(()),
        },
        Input::Button { button, .. } => send(&[here(flag(*button, false), 0)]),
        Input::Scroll { x, y } => {
            let mut cursor = POINT::default();
            unsafe { GetCursorPos(&mut cursor) }?;
            if !hits(target, cursor) {
                return Ok(());
            }
            let notches = |n: f32| (n * WHEEL_DELTA).round() as i32;
            let mut events = Vec::new();
            if *y != 0.0 {
                events.push(here(MOUSEEVENTF_WHEEL, notches(*y)));
            }
            // Windows scrolls right for a positive horizontal wheel; ours scrolls left.
            if *x != 0.0 {
                events.push(here(MOUSEEVENTF_HWHEEL, notches(-*x)));
            }
            send(&events)
        }
        Input::Key { key, down } => {
            let code = scan_code(key).context("key not allowed")?;
            if *down && !focused(target) {
                return Ok(());
            }
            send(&[keyboard(code, *down)])
        }
    }
}

fn flag(button: Button, down: bool) -> MOUSE_EVENT_FLAGS {
    match (button, down) {
        (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (Button::Left, false) => MOUSEEVENTF_LEFTUP,
        (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
        (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    }
}

fn hwnd(target: &Target) -> Option<HWND> {
    match target {
        Target::Window(handle) => Some(window(*handle)),
        Target::Monitor(_) => None,
    }
}

/// The shared monitor or window on the desktop, in physical pixels.
fn bounds(target: Target) -> Result<RECT> {
    match target {
        Target::Monitor(handle) => {
            let mut info = MONITORINFO {
                cbSize: size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            unsafe { GetMonitorInfoW(monitor(handle), &mut info) }
                .ok()
                .context("shared monitor is gone")?;
            Ok(info.rcMonitor)
        }
        Target::Window(handle) => {
            let hwnd = window(handle);
            ensure!(
                !unsafe { IsIconic(hwnd) }.as_bool(),
                "shared window is minimized"
            );
            // The visible frame, which is what the capture shows, without the
            // invisible resize border `GetWindowRect` includes.
            let mut rect = RECT::default();
            unsafe {
                DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    (&mut rect as *mut RECT).cast::<c_void>(),
                    size_of::<RECT>() as u32,
                )
            }
            .context("shared window is gone")?;
            Ok(rect)
        }
    }
}

/// A point on the source as absolute `SendInput` coordinates, if it lands on the share.
fn place(target: Target, point: Option<(f32, f32)>) -> Result<Option<(i32, i32)>> {
    let Some((x, y)) = point else {
        return Ok(None);
    };
    let rect = bounds(target)?;
    let along =
        |from: i32, to: i32, at: f32| from + ((to - from - 1).max(0) as f32 * at).round() as i32;
    let pixel = POINT {
        x: along(rect.left, rect.right, x),
        y: along(rect.top, rect.bottom, y),
    };
    if !hits(target, pixel) {
        return Ok(None);
    }
    // Absolute coordinates run from 0 to 65535 across the whole virtual desktop.
    let metric = |index| unsafe { GetSystemMetrics(index) };
    let scale = |p: i32, origin: i32, size: i32| {
        (i64::from(p - origin) * 65535 / i64::from((size - 1).max(1))) as i32
    };
    Ok(Some((
        scale(
            pixel.x,
            metric(SM_XVIRTUALSCREEN),
            metric(SM_CXVIRTUALSCREEN),
        ),
        scale(
            pixel.y,
            metric(SM_YVIRTUALSCREEN),
            metric(SM_CYVIRTUALSCREEN),
        ),
    )))
}

fn ours(window: HWND) -> bool {
    let mut process = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut process)) };
    process == unsafe { GetCurrentProcessId() }
}

/// Whether the window under this point is the share (any window, for a monitor),
/// and not one of ours.
fn hits(target: Target, point: POINT) -> bool {
    let hit = unsafe { WindowFromPoint(point) };
    if hit.is_invalid() {
        return false;
    }
    let root = unsafe { GetAncestor(hit, GA_ROOT) };
    !ours(root) && hwnd(&target).is_none_or(|shared| root == shared)
}

/// Whether keys would go to the share: the shared window, or a window on the shared
/// monitor, has the focus, and it isn't one of ours.
fn focused(target: Target) -> bool {
    let front = unsafe { GetForegroundWindow() };
    if front.is_invalid() || ours(front) {
        return false;
    }
    match target {
        Target::Window(handle) => {
            let root = unsafe { GetAncestor(front, GA_ROOT) };
            root == window(handle)
        }
        Target::Monitor(handle) => {
            let on = unsafe { MonitorFromWindow(front, MONITOR_DEFAULTTONULL) };
            on == monitor(handle)
        }
    }
}

fn mouse((dx, dy): (i32, i32), flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// A button release or wheel turn where the pointer is now.
fn here(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                mouseData: data as u32,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

/// A physical key by scan code, so the sharer's own keyboard layout applies.
fn keyboard(code: u16, down: bool) -> INPUT {
    let mut flags = KEYEVENTF_SCANCODE;
    if code > 0xFF {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: code & 0xFF,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(events: &[INPUT]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(events, size_of::<INPUT>() as i32) };
    ensure!(
        sent as usize == events.len(),
        "Windows blocked the remote input"
    );
    Ok(())
}
