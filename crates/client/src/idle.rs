//! How long the computer has had no input at all, from the system, so working in
//! another app doesn't count as being away. Only the idle duration is read, never
//! the input itself, and nothing leaves the computer.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Ask the system this often.
const POLL: Duration = Duration::from_secs(5);

/// The system's idle time, read on a thread of its own: D-Bus and X11 calls can
/// block, and the window must not wait for them.
pub struct SystemIdle {
    /// The last reading and when it was taken; `None` while unknown.
    reading: Arc<Mutex<Option<(Duration, Instant)>>>,
    /// Dropping it ends the thread.
    _stop: Option<std::sync::mpsc::Sender<()>>,
}

impl SystemIdle {
    /// Start reading, on desktops; phones keep counting input in the app.
    pub fn start() -> Self {
        let reading = Arc::new(Mutex::new(None));
        if cfg!(test) || !cfg!(any(target_os = "linux", windows, target_os = "macos")) {
            return Self {
                reading,
                _stop: None,
            };
        }
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let shared = reading.clone();
        let spawned = std::thread::Builder::new()
            .name("system-idle".into())
            .spawn(move || {
                let mut probe = match Probe::open() {
                    Ok((probe, source)) => {
                        tracing::info!(source, "reading the system idle time");
                        probe
                    }
                    Err(reason) => {
                        tracing::warn!(%reason, "no system idle time, counting only input in the app");
                        return;
                    }
                };
                let mut failing = false;
                loop {
                    let read = probe.read();
                    if let Err(reason) = &read
                        && !failing
                    {
                        tracing::warn!(%reason, "can't read the system idle time");
                    }
                    failing = read.is_err();
                    *shared.lock().unwrap_or_else(|e| e.into_inner()) =
                        read.ok().map(|idle| (idle, Instant::now()));
                    if !matches!(
                        stopped.recv_timeout(POLL),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ) {
                        return;
                    }
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "can't start reading the system idle time");
        }
        Self {
            reading,
            _stop: Some(stop),
        }
    }

    /// Seconds since the last input anywhere on the computer, if the system says.
    pub fn seconds(&self) -> Option<f64> {
        let reading = *self.reading.lock().unwrap_or_else(|e| e.into_inner());
        reading.map(|(idle, at)| (idle + at.elapsed()).as_secs_f64())
    }
}

#[cfg(target_os = "linux")]
enum Probe {
    X11 {
        conn: Box<x11rb::rust_connection::RustConnection>,
        root: u32,
    },
    DBus {
        conn: zbus::blocking::Connection,
        call: &'static DBusIdle,
    },
}

/// A D-Bus method that answers the session's idle time in milliseconds.
#[cfg(target_os = "linux")]
struct DBusIdle {
    source: &'static str,
    service: &'static str,
    path: &'static str,
    method: &'static str,
    /// The method answers a `u64` rather than a `u32`.
    wide: bool,
}

/// GNOME first: its shell also owns `org.freedesktop.ScreenSaver` but has no
/// `GetSessionIdleTime` there. KDE and others have the freedesktop one.
#[cfg(target_os = "linux")]
const DBUS_IDLE: [DBusIdle; 2] = [
    DBusIdle {
        source: "GNOME idle monitor",
        service: "org.gnome.Mutter.IdleMonitor",
        path: "/org/gnome/Mutter/IdleMonitor/Core",
        method: "GetIdletime",
        wide: true,
    },
    DBusIdle {
        source: "freedesktop screen saver",
        service: "org.freedesktop.ScreenSaver",
        path: "/org/freedesktop/ScreenSaver",
        method: "GetSessionIdleTime",
        wide: false,
    },
];

#[cfg(target_os = "linux")]
impl Probe {
    fn open() -> Result<(Self, &'static str), String> {
        let mut reasons = Vec::new();
        // Under Wayland, XWayland's idle time only counts input sent to X11 apps.
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            match Self::x11() {
                Ok(mut probe) => match probe.read() {
                    Ok(_) => return Ok((probe, "X11 screen saver")),
                    Err(reason) => reasons.push(reason),
                },
                Err(reason) => reasons.push(reason),
            }
        }
        match zbus::blocking::Connection::session() {
            Ok(conn) => {
                for call in &DBUS_IDLE {
                    let mut probe = Self::DBus {
                        conn: conn.clone(),
                        call,
                    };
                    match probe.read() {
                        Ok(_) => return Ok((probe, call.source)),
                        Err(reason) => reasons.push(reason),
                    }
                }
            }
            Err(error) => reasons.push(format!("session bus: {error}")),
        }
        Err(reasons.join("; "))
    }

    fn x11() -> Result<Self, String> {
        use x11rb::connection::Connection as _;
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("X11: {e}"))?;
        let root = conn.setup().roots.get(screen).ok_or("X11: no screen")?.root;
        Ok(Self::X11 {
            conn: Box::new(conn),
            root,
        })
    }

    fn read(&mut self) -> Result<Duration, String> {
        match self {
            Self::X11 { conn, root } => {
                use x11rb::protocol::screensaver::ConnectionExt as _;
                let info = conn
                    .screensaver_query_info(*root)
                    .map_err(|e| format!("X11 screen saver: {e}"))?
                    .reply()
                    .map_err(|e| format!("X11 screen saver: {e}"))?;
                Ok(Duration::from_millis(info.ms_since_user_input.into()))
            }
            Self::DBus { conn, call } => {
                let fail = |e: zbus::Error| format!("{}: {e}", call.source);
                let reply = conn
                    .call_method(
                        Some(call.service),
                        call.path,
                        Some(call.service),
                        call.method,
                        &(),
                    )
                    .map_err(fail)?;
                let body = reply.body();
                let ms = if call.wide {
                    body.deserialize::<u64>().map_err(fail)?
                } else {
                    body.deserialize::<u32>().map_err(fail)?.into()
                };
                Ok(Duration::from_millis(ms))
            }
        }
    }
}

#[cfg(windows)]
struct Probe;

#[cfg(windows)]
impl Probe {
    fn open() -> Result<(Self, &'static str), String> {
        Probe.read().map(|_| (Probe, "GetLastInputInfo"))
    }

    fn read(&mut self) -> Result<Duration, String> {
        use windows::Win32::System::SystemInformation::GetTickCount;
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY: `info` is a valid LASTINPUTINFO with its size set, as the call needs.
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            return Err(format!(
                "GetLastInputInfo: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Both are milliseconds since boot in 32 bits, wrapping after 49.7 days.
        // SAFETY: GetTickCount has no preconditions.
        let now = unsafe { GetTickCount() };
        Ok(Duration::from_millis(now.wrapping_sub(info.dwTime).into()))
    }
}

#[cfg(target_os = "macos")]
struct Probe;

#[cfg(target_os = "macos")]
impl Probe {
    fn open() -> Result<(Self, &'static str), String> {
        Probe
            .read()
            .map(|_| (Probe, "CGEventSourceSecondsSinceLastEventType"))
    }

    fn read(&mut self) -> Result<Duration, String> {
        use objc2_core_graphics::{CGEventSource, CGEventSourceStateID, CGEventType};
        // kCGAnyInputEventType: keyboard, mouse and tablet alike.
        let any_input = CGEventType(u32::MAX);
        let seconds = CGEventSource::seconds_since_last_event_type(
            CGEventSourceStateID::CombinedSessionState,
            any_input,
        );
        Duration::try_from_secs_f64(seconds)
            .map_err(|e| format!("CGEventSourceSecondsSinceLastEventType: {seconds}: {e}"))
    }
}

/// Phones and other systems: never started, see `SystemIdle::start`.
#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
struct Probe;

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
impl Probe {
    fn open() -> Result<(Self, &'static str), String> {
        Err("not available on this system".into())
    }

    fn read(&mut self) -> Result<Duration, String> {
        unreachable!("never opened")
    }
}
