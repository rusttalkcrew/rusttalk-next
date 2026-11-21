//! Wayland input for an opted-in Hyprland monitor share.

use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::{fs::FileExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::SeqCst},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use rustix::{
    event::{PollFd, PollFlags, poll},
    net::sockopt::socket_peercred,
    time::{ClockId, clock_gettime},
};
use serde::Deserialize;
use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    protocol::{wl_callback, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat},
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1 as idle, ext_idle_notifier_v1 as notifier,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1 as keyboard_manager, zwp_virtual_keyboard_v1 as keyboard,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1 as pointer_manager, zwlr_virtual_pointer_v1 as pointer,
};
use xkbcommon::xkb;

#[path = "hyprland_keys.rs"]
mod keys;
#[path = "hyprland_scope.rs"]
mod scope;

use crate::control::{Button, Input};
use scope::{Monitor, Scope, Verified};

const IO_TIMEOUT: Duration = Duration::from_millis(100);
const LOCAL_PAUSE: Duration = Duration::from_millis(500);
const HELD_CHECK: Duration = Duration::from_millis(100);
const POINTER_SNAPSHOT: &str = "[[BATCH]]j/monitors;j/locked;j/cursorpos";
const BIND_SNAPSHOT: &str = "[[BATCH]]j/submap;j/binds";
// j/layers omits interactivity on releases that expose it through Lua's layer objects.
// This fixed read-only query joins by layer address and accepts no interpolated input.
const KEY_SNAPSHOT: &str = concat!(
    "[[BATCH]]j/monitors;j/locked;j/cursorpos;j/activewindow;j/layers;j/submap;j/binds;",
    r#"repl return (function() local out = {} for _, layer in ipairs(hl.get_layers()) do "#,
    r#"local mode = layer.keyboard_interactivity or layer.interactivity "#,
    r#"if layer.mapped and type(mode) == "number" then "#,
    r#"table.insert(out, string.format('"%s":%d', layer.address, mode)) end end "#,
    r#"return "{" .. table.concat(out, ",") .. "}" end)()"#,
);

fn event_time() -> u32 {
    let now = clock_gettime(ClockId::Monotonic);
    (now.tv_sec as u32)
        .wrapping_mul(1000)
        .wrapping_add((now.tv_nsec / 1_000_000) as u32)
}

pub(super) fn socket() -> Option<PathBuf> {
    let instance = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if instance.is_empty() || instance.contains('/') || matches!(instance.as_str(), "." | "..") {
        return None;
    }
    Some(
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)?
            .join("hypr")
            .join(instance)
            .join(".socket.sock"),
    )
}

fn query(socket: &Path, server: i32, command: &str) -> Result<String> {
    let mut stream = UnixStream::connect(socket).context("connect to Hyprland IPC")?;
    ensure!(
        socket_peercred(&stream)?.pid.as_raw_pid() == server,
        "Wayland and Hyprland IPC belong to different sessions"
    );
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(command.as_bytes())?;
    let mut result = String::new();
    (&mut stream).take(256 * 1024).read_to_string(&mut result)?;
    ensure!(result.len() < 256 * 1024, "Hyprland IPC response too large");
    Ok(result)
}

struct Event {
    session: u64,
    received: Instant,
    input: Input,
}

pub(super) struct Control {
    generation: Arc<AtomicU64>,
    input: SyncSender<Event>,
    overflow: Arc<AtomicBool>,
}

impl Control {
    pub(super) async fn open(name: String) -> Result<Self> {
        let socket = socket().context("no Hyprland session")?;
        let generation = Arc::new(AtomicU64::new(0));
        let (input, events) = mpsc::sync_channel(1);
        let (ready, started) = tokio::sync::oneshot::channel();
        let worker_generation = generation.clone();
        let overflow = Arc::new(AtomicBool::new(false));
        let worker_overflow = overflow.clone();
        std::thread::Builder::new()
            .name("hyprland-input".into())
            .spawn(move || {
                let setup = (|| {
                    let wayland = Wayland::open(name.clone())?;
                    let server = socket_peercred(&wayland.connection)?.pid.as_raw_pid();
                    let monitor = Monitor::find(&query(&socket, server, "j/monitors")?, &name)?;
                    let lock: serde_json::Value =
                        serde_json::from_str(&query(&socket, server, "j/locked")?)?;
                    ensure!(
                        lock["locked"].is_boolean(),
                        "Hyprland lock status is unavailable"
                    );
                    Ok((wayland, monitor))
                })();
                match setup {
                    Ok((wayland, monitor)) => {
                        if ready.send(Ok(())).is_ok()
                            && let Err(error) = run(
                                wayland,
                                &socket,
                                &monitor,
                                worker_generation,
                                &worker_overflow,
                                events,
                            )
                        {
                            tracing::warn!(%error, "Hyprland remote input ended");
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })?;
        started.await??;
        Ok(Self {
            generation,
            input,
            overflow,
        })
    }

    pub(super) fn available(&self) -> bool {
        self.generation.load(SeqCst) != u64::MAX
    }

    pub(super) fn set_active(&self, active: bool) {
        let _ = self.generation.try_update(SeqCst, SeqCst, |current| {
            (current != u64::MAX && (current % 2 == 1) != active).then(|| current + 1)
        });
    }

    pub(super) fn inject(
        &self,
        input: &Input,
        point: impl Fn(f32, f32) -> Option<(f32, f32)>,
    ) -> Result<()> {
        let session = self.generation.load(SeqCst);
        ensure!(
            session % 2 == 1 && session != u64::MAX,
            "remote input is inactive"
        );
        let mut input = input.clone();
        match &mut input {
            Input::Move { x, y }
            | Input::Button {
                x, y, down: true, ..
            } => {
                (*x, *y) = point(*x, *y).context("remote pointer outside the shared picture")?;
            }
            _ => (),
        }
        // One in flight is enough. In particular, a burst must never replay after a pause.
        self.input
            .try_send(Event {
                session,
                received: Instant::now(),
                input,
            })
            .map_err(|error| {
                if let TrySendError::Full(event) = error
                    && matches!(event.input, Input::Key { .. } | Input::Button { .. })
                {
                    self.overflow.store(true, SeqCst);
                }
                anyhow::anyhow!("Hyprland input busy or disconnected")
            })
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        self.generation.store(u64::MAX, SeqCst);
    }
}

struct Wayland {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
}

#[derive(Default)]
struct State {
    name: String,
    seat: Option<wl_seat::WlSeat>,
    output: Option<(u32, wl_output::WlOutput)>,
    pointer: Option<pointer_manager::ZwlrVirtualPointerManagerV1>,
    keyboard: Option<keyboard_manager::ZwpVirtualKeyboardManagerV1>,
    notifier: Option<notifier::ExtIdleNotifierV1>,
    keymap: Option<(OwnedFd, u32)>,
    idle: bool,
    idle_for_pause: bool,
    activity: u64,
    synced: bool,
}

impl Wayland {
    fn open(name: String) -> Result<Self> {
        let connection = Connection::connect_to_env()?;
        let queue = connection.new_event_queue();
        let _registry = connection.display().get_registry(&queue.handle(), ());
        let mut this = Self {
            connection,
            queue,
            state: State {
                name,
                ..State::default()
            },
        };
        this.sync()?;
        let qh = this.queue.handle();
        let state = &this.state;
        let seat = state.seat.as_ref().context("Wayland seat is missing")?;
        let _keyboard = seat.get_keyboard(&qh, ());
        // Hyprland counts virtual input as activity too. Short notifications let us
        // account for our own activity without blinding the 500 ms local-input pause.
        let notifier = state
            .notifier
            .as_ref()
            .context("Wayland input idle notifications are unavailable")?;
        let _idle = notifier.get_input_idle_notification(1, seat, &qh, false);
        let _pause = notifier.get_input_idle_notification(500, seat, &qh, true);
        this.sync()?;
        ensure!(
            this.state.output.is_some()
                && this.state.pointer.is_some()
                && this.state.keyboard.is_some()
                && this.state.keymap.is_some(),
            "Wayland output, virtual input or seat keymap is unavailable"
        );
        Ok(this)
    }

    fn sync(&mut self) -> Result<()> {
        let connection = self.queue.handle();
        self.state.synced = false;
        self.queue.dispatch_pending(&mut self.state)?;
        // An explicit, bounded round trip keeps focus checks and shutdown responsive.
        self.connection.display().sync(&connection, ());
        self.queue.flush()?;
        let deadline = Instant::now() + IO_TIMEOUT;
        while !self.state.synced {
            if let Some(guard) = self.queue.prepare_read() {
                let mut fds = [PollFd::from_borrowed_fd(
                    guard.connection_fd(),
                    PollFlags::IN,
                )];
                let left = deadline
                    .checked_duration_since(Instant::now())
                    .context("Wayland input round trip timed out")?;
                ensure!(
                    poll(&mut fds, Some(&left.try_into()?))? > 0,
                    "Wayland input round trip timed out"
                );
                guard.read()?;
            }
            self.queue.dispatch_pending(&mut self.state)?;
        }
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use wl_registry::Event::*;
        match event {
            Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()))
                }
                "wl_output" if version >= 4 => {
                    let _: wl_output::WlOutput = registry.bind(name, 4, qh, name);
                }
                "zwlr_virtual_pointer_manager_v1" if version >= 2 => {
                    state.pointer = Some(registry.bind(name, 2, qh, ()))
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.keyboard = Some(registry.bind(name, 1, qh, ()))
                }
                "ext_idle_notifier_v1" if version >= 2 => {
                    state.notifier = Some(registry.bind(name, 2, qh, ()))
                }
                _ => (),
            },
            GlobalRemove { name } if state.output.as_ref().is_some_and(|(id, _)| *id == name) => {
                state.output = None;
                state.name.clear(); // A replug must not attach this share to a replacement output.
            }
            _ => (),
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event
            && name == state.name
        {
            state.output = Some((*id, output.clone()));
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            state.keymap = (format
                == wayland_client::WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)
                && (1..=1048576).contains(&size))
            .then_some((fd, size));
        }
    }
}
impl Dispatch<idle::ExtIdleNotificationV1, bool> for State {
    fn event(
        state: &mut Self,
        _: &idle::ExtIdleNotificationV1,
        event: idle::Event,
        long: &bool,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let idle = matches!(event, idle::Event::Idled);
        if *long {
            state.idle_for_pause = idle;
        } else {
            state.idle = idle;
            if !idle {
                state.activity += 1;
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.synced = true;
    }
}
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore notifier::ExtIdleNotifierV1);
delegate_noop!(State: ignore keyboard_manager::ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore keyboard::ZwpVirtualKeyboardV1);
delegate_noop!(State: ignore pointer_manager::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore pointer::ZwlrVirtualPointerV1);

struct Devices {
    pointer: pointer::ZwlrVirtualPointerV1,
    keyboard: keyboard::ZwpVirtualKeyboardV1,
    xkb: xkb::State,
    focus: Option<String>,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<Button>,
}

impl Devices {
    fn open(wayland: &Wayland, socket: &Path, server: i32) -> Result<Self> {
        #[derive(Deserialize)]
        struct Keyboards {
            keyboards: Vec<Keyboard>,
        }
        #[derive(Deserialize)]
        struct Keyboard {
            main: bool,
            active_layout_index: Option<u32>,
        }
        // Wayland only sends modifier/group events to the focused client.
        let group = serde_json::from_str::<Keyboards>(&query(socket, server, "j/devices")?)?
            .keyboards
            .into_iter()
            .find(|keyboard| keyboard.main)
            .and_then(|keyboard| keyboard.active_layout_index)
            .context("active Hyprland keyboard layout unavailable")?;
        let state = &wayland.state;
        let (_, output) = state
            .output
            .as_ref()
            .context("shared Wayland output disappeared")?;
        let seat = state.seat.as_ref().context("Wayland seat disappeared")?;
        let (fd, size) = state
            .keymap
            .as_ref()
            .context("seat keyboard map disappeared")?;
        let mut map = vec![0; *size as usize];
        File::from(fd.try_clone()?).read_exact_at(&mut map, 0)?;
        let map = String::from_utf8(map)?;
        let xkb = xkb::Keymap::new_from_string(
            &xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            map.trim_end_matches('\0').into(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .context("invalid Wayland seat keymap")?;
        ensure!(
            group < xkb.num_layouts(),
            "active layout missing from the Wayland seat keymap"
        );
        let mut xkb = xkb::State::new(&xkb);
        xkb.update_mask(0, 0, 0, 0, 0, group);
        let qh = wayland.queue.handle();
        let pointer = state
            .pointer
            .as_ref()
            .context("virtual pointer manager disappeared")?
            .create_virtual_pointer_with_output(Some(seat), Some(output), &qh, ());
        let keyboard = state
            .keyboard
            .as_ref()
            .context("virtual keyboard manager disappeared")?
            .create_virtual_keyboard(seat, &qh, ());
        keyboard.keymap(1, fd.as_fd(), *size);
        keyboard.modifiers(0, 0, 0, group);
        Ok(Self {
            pointer,
            keyboard,
            xkb,
            focus: None,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
        })
    }

    fn motion(&self, time: u32, x: f32, y: f32) {
        const EXTENT: u32 = 65536;
        let scale = |n: f32| ((n * EXTENT as f32) as u32).min(EXTENT - 1);
        self.pointer
            .motion_absolute(time, scale(x), scale(y), EXTENT, EXTENT);
        self.pointer.frame();
    }

    fn input(&mut self, input: &Input, time: u32) {
        match *input {
            Input::Move { x, y } => self.motion(time, x, y),
            Input::Button { button, down, .. } => {
                let changed = if down {
                    self.buttons.insert(button)
                } else {
                    self.buttons.remove(&button)
                };
                if changed {
                    self.button(time, button, down);
                }
            }
            Input::Scroll { x, y } => {
                for (axis, n) in [
                    (wl_pointer::Axis::HorizontalScroll, x),
                    (wl_pointer::Axis::VerticalScroll, y),
                ] {
                    if n != 0.0 {
                        let distance = f64::from(-n) * 15.0;
                        if n.abs() >= 1.0 {
                            self.pointer
                                .axis_discrete(time, axis, distance, -n.round() as i32);
                        } else {
                            self.pointer.axis(time, axis, distance);
                        }
                        self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
                    }
                }
                self.pointer.frame();
            }
            Input::Key { ref key, down } => {
                let Some(code) = scope::evdev(key) else {
                    return;
                };
                let changed = if down {
                    self.keys.insert(code)
                } else {
                    self.keys.remove(&code)
                };
                if changed {
                    self.key(time, code, down);
                }
            }
        }
    }

    fn button(&self, time: u32, button: Button, down: bool) {
        let code = match button {
            Button::Left => 0x110,
            Button::Right => 0x111,
            Button::Middle => 0x112,
        };
        let state = if down {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        self.pointer.button(time, code, state);
        self.pointer.frame();
    }

    fn key(&mut self, time: u32, code: u32, down: bool) {
        self.keyboard.key(time, code, u32::from(down));
        self.xkb.update_key(
            (code + 8).into(),
            if down {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            },
        );
        self.keyboard.modifiers(
            self.xkb.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            self.xkb.serialize_mods(xkb::STATE_MODS_LATCHED),
            self.xkb.serialize_mods(xkb::STATE_MODS_LOCKED),
            self.xkb.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        );
    }

    fn held(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }

    fn valid(&self, verified: &Verified) -> bool {
        self.focus
            .as_ref()
            .is_none_or(|focus| verified.focus.as_ref() == Some(focus))
            && verified
                .binds
                .as_ref()
                .is_some_and(|binds| binds.allows(&self.xkb, &self.keys, None))
    }
}

impl Drop for Devices {
    fn drop(&mut self) {
        let time = event_time();
        // Clear the virtual modifier mask before cleanup so releases carry no remote chord.
        if !self.keys.is_empty() {
            self.keyboard.modifiers(
                0,
                0,
                0,
                self.xkb.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
            );
        }
        for &key in &self.keys {
            self.keyboard.key(time, key, 0);
        }
        for &button in &self.buttons {
            self.button(time, button, false);
        }
        self.keyboard.destroy();
        self.pointer.destroy();
    }
}

fn revalidate_held<T>(
    devices: &mut Option<T>,
    now: Instant,
    next: &mut Instant,
    held: impl FnOnce(&T) -> bool,
    valid: impl FnOnce(&T) -> bool,
) -> bool {
    if now < *next || !devices.as_ref().is_some_and(held) {
        return false;
    }
    *next = now + HELD_CHECK;
    if devices.as_ref().is_some_and(valid) {
        return false;
    }
    devices.take().is_some()
}

fn run(
    mut wayland: Wayland,
    socket: &Path,
    monitor: &Monitor,
    generation: Arc<AtomicU64>,
    overflow: &AtomicBool,
    events: Receiver<Event>,
) -> Result<()> {
    let result = run_input(&mut wayland, socket, monitor, &generation, overflow, events);
    generation.store(u64::MAX, SeqCst);
    // Destroy requests and held releases are queued by Devices::drop.
    let _ = wayland.queue.flush();
    result
}

fn run_input(
    wayland: &mut Wayland,
    socket: &Path,
    monitor: &Monitor,
    generation: &AtomicU64,
    overflow: &AtomicBool,
    events: Receiver<Event>,
) -> Result<()> {
    let server = socket_peercred(&wayland.connection)?.pid.as_raw_pid();
    let mut paused = Some(Instant::now());
    let mut session = 0;
    let mut devices: Option<Devices> = None;
    let mut activity = wayland.state.activity;
    let mut next_held_check = Instant::now();
    let verify = |scope| {
        let command = match scope {
            Scope::Keyboard => KEY_SNAPSHOT,
            _ => POINTER_SNAPSHOT,
        };
        match query(socket, server, command) {
            Ok(snapshot) => {
                let mut verified = scope::verify(&snapshot, monitor, scope)?;
                if let Some(binds) = &mut verified.binds
                    && binds.needs_display_keys()
                {
                    let text = query(socket, server, "binds").ok()?;
                    binds.fill_display_keys(&text)?;
                }
                Some(verified)
            }
            Err(error) => {
                tracing::debug!(%error, "Hyprland input check failed");
                None
            }
        }
    };
    let observe =
        |wayland: &mut Wayland, activity: &mut u64, paused: &mut Option<Instant>, own: u64| {
            wayland.sync()?;
            if wayland.state.activity - *activity > own {
                *paused = Some(Instant::now());
            }
            *activity = wayland.state.activity;
            ensure!(
                wayland.state.output.is_some(),
                "shared Wayland output disconnected"
            );
            Ok::<_, anyhow::Error>(())
        };

    loop {
        let event = match events.recv_timeout(Duration::from_millis(15)) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let current = generation.load(SeqCst);
        if current == u64::MAX {
            return Ok(());
        }
        if current != session {
            if devices.take().is_some() {
                observe(wayland, &mut activity, &mut paused, 1)?;
            }
            session = current;
            paused = Some(Instant::now());
            if session % 2 == 1 {
                let binds = query(socket, server, BIND_SNAPSHOT).ok().and_then(|reply| {
                    keys::Binds::parse(&mut serde_json::Deserializer::from_str(&reply))
                });
                if binds.is_none() {
                    tracing::debug!("Hyprland binds unavailable when control started");
                }
            }
        }
        if session % 2 == 0 {
            continue;
        }
        observe(wayland, &mut activity, &mut paused, 0)?;
        if wayland.state.idle_for_pause && paused.is_some_and(|at| at.elapsed() >= LOCAL_PAUSE) {
            paused = None;
        }
        if overflow.swap(false, SeqCst) || paused.is_some() {
            if devices.as_ref().is_some_and(Devices::held) {
                drop(devices.take());
                observe(wayland, &mut activity, &mut paused, 1)?;
            }
            continue;
        }
        if revalidate_held(
            &mut devices,
            Instant::now(),
            &mut next_held_check,
            Devices::held,
            |devices| verify(Scope::Keyboard).is_some_and(|verified| devices.valid(&verified)),
        ) {
            tracing::debug!("Hyprland held input lost verified scope");
            observe(wayland, &mut activity, &mut paused, 1)?;
            continue;
        }

        let Some(event) = event.filter(|event| event.session == session) else {
            continue;
        };
        let time = event_time();
        let scope = || Scope::from(&event.input);
        let verified = (event.received.elapsed() < IO_TIMEOUT)
            .then(|| verify(scope()))
            .flatten();
        let Some(mut verified) = verified else {
            if devices.take().is_some() {
                observe(wayland, &mut activity, &mut paused, 1)?;
            }
            continue;
        };
        if devices.is_none() {
            if generation.load(SeqCst) != session {
                continue;
            }
            devices = Some(Devices::open(wayland, socket, server)?);
            observe(wayland, &mut activity, &mut paused, 1)?;
            let Some(fresh) = verify(scope()) else {
                drop(devices.take());
                observe(wayland, &mut activity, &mut paused, 1)?;
                continue;
            };
            verified = fresh;
        }
        observe(wayland, &mut activity, &mut paused, 0)?;
        if !wayland.state.idle
            || paused.is_some()
            || generation.load(SeqCst) != session
            || event.received.elapsed() >= IO_TIMEOUT
        {
            if matches!(
                event.input,
                Input::Key { down: false, .. } | Input::Button { down: false, .. }
            ) && devices.take().is_some()
            {
                observe(wayland, &mut activity, &mut paused, 1)?;
            }
            continue;
        }
        let active = devices.as_mut().expect("virtual input devices initialized");
        if let Input::Key { ref key, down } = event.input {
            if !active.valid(&verified) {
                drop(devices.take());
                observe(wayland, &mut activity, &mut paused, 1)?;
                continue;
            }
            let Some(code) = scope::evdev(key) else {
                continue;
            };
            if (down && scope::blocked_combo(code, &active.keys))
                || !verified.binds.as_ref().is_some_and(|binds| {
                    binds.allows(&active.xkb, &active.keys, Some((code, down)))
                })
            {
                if !down && active.keys.contains(&code) {
                    drop(devices.take());
                    observe(wayland, &mut activity, &mut paused, 1)?;
                }
                continue;
            }
            active.focus = verified.focus;
        }
        if let Input::Button {
            down: true, x, y, ..
        } = event.input
        {
            active.motion(time, x, y);
            observe(wayland, &mut activity, &mut paused, 1)?;
            let scoped = verify(scope()).is_some();
            observe(wayland, &mut activity, &mut paused, 0)?;
            if !scoped
                || paused.is_some()
                || generation.load(SeqCst) != session
                || event.received.elapsed() >= IO_TIMEOUT
            {
                drop(devices.take());
                observe(wayland, &mut activity, &mut paused, 1)?;
                continue;
            }
        }
        let was_held = active.held();
        active.input(&event.input, time);
        if !was_held && active.held() {
            next_held_check = Instant::now() + HELD_CHECK;
        }
        if active.keys.is_empty() {
            active.focus = None;
        }
        observe(wayland, &mut activity, &mut paused, 1)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn idle_held_key_is_released_when_scope_changes() {
        struct Pressed<'a>(&'a Cell<bool>);
        impl Drop for Pressed<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }

        let monitors = include_str!("fixtures/monitors.json");
        let target = Monitor::find(monitors, "HDMI-A-1").unwrap();
        let snapshot = |locked| {
            [
                monitors,
                locked,
                include_str!("fixtures/cursorpos.json"),
                include_str!("fixtures/activewindow.json"),
                include_str!("fixtures/layers.json"),
                r#""default""#,
                include_str!("fixtures/binds.json"),
            ]
            .join("\n")
        };
        let check = |locked, _: &Pressed<'_>| {
            scope::verify(&snapshot(locked), &target, Scope::Keyboard).is_some()
        };
        let down = Cell::new(true);
        let mut device = Some(Pressed(&down));
        let start = Instant::now();
        let mut next = start;
        assert!(!revalidate_held(
            &mut device,
            start,
            &mut next,
            |_| true,
            |device| check(include_str!("fixtures/locked.json"), device)
        ));
        assert!(down.get());
        assert!(revalidate_held(
            &mut device,
            start + HELD_CHECK,
            &mut next,
            |_| true,
            |device| check(r#"{"locked":true}"#, device)
        ));
        assert!(!down.get());
        assert!(device.is_none());
    }
}
