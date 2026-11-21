//! Global shortcuts: mute, deafen, push-to-talk and screen sharing while RustTalk is
//! in the background. Windows, macOS and X11 bind keys through the `global-hotkey`
//! crate; on Wayland apps can't, so the desktop binds them through its global
//! shortcuts portal (`portal`). On Linux a running RustTalk also takes
//! `rusttalk --toggle-mute` and friends (`ipc`), for desktops without the portal.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    sync::{Mutex, Once, mpsc},
    thread,
};

use eframe::egui::{self, Event, Key, Modifiers, Ui, Vec2};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers as Mods},
};
use serde::{Deserialize, Serialize};

use crate::{
    audio::{Audio, lock},
    call::{self, Sharing},
    home::{Home, Tab, user_bar},
    net::Command,
    screen,
    ui::{self, Look},
};

#[cfg(target_os = "linux")]
mod ipc;
#[cfg(target_os = "linux")]
mod portal;

#[cfg(target_os = "linux")]
pub use ipc::send;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    ToggleMute,
    ToggleDeafen,
    PushToTalk,
    ShareWindow,
    StopSharing,
}

impl Action {
    pub const ALL: [Self; 5] = [
        Self::ToggleMute,
        Self::ToggleDeafen,
        Self::PushToTalk,
        Self::ShareWindow,
        Self::StopSharing,
    ];

    /// The portal's shortcut ID and the command line flag: `rusttalk --toggle-mute`.
    pub fn id(self) -> &'static str {
        match self {
            Self::ToggleMute => "toggle-mute",
            Self::ToggleDeafen => "toggle-deafen",
            Self::PushToTalk => "push-to-talk",
            Self::ShareWindow => "share-window",
            Self::StopSharing => "stop-sharing",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ToggleMute => t!("hotkeys.toggle_mute"),
            Self::ToggleDeafen => t!("hotkeys.toggle_deafen"),
            Self::PushToTalk => t!("call.push_to_talk"),
            Self::ShareWindow => t!("hotkeys.share_window"),
            Self::StopSharing => t!("hotkeys.stop_sharing"),
        }
    }

    fn help(self) -> &'static str {
        match self {
            Self::ToggleMute | Self::ToggleDeafen => "",
            Self::PushToTalk => t!("hotkeys.push_to_talk_help"),
            // Linux can't see other apps' windows; the desktop's picker asks instead.
            Self::ShareWindow if cfg!(target_os = "linux") => t!("hotkeys.share_window_linux"),
            Self::ShareWindow => t!("hotkeys.share_window_help"),
            Self::StopSharing => t!("hotkeys.stop_sharing_help"),
        }
    }

    /// The commands to bind in a desktop without the portal.
    fn commands(self) -> String {
        match self {
            Self::PushToTalk => t!("hotkeys.push_to_talk_commands").to_owned(),
            _ => format!("rusttalk --{}", self.id()),
        }
    }
}

/// A command from `rusttalk --<id>`: the action and whether its key went down.
/// `--push-to-talk-release` lets go of push-to-talk.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn command(text: &str) -> Option<(Action, bool)> {
    let (id, pressed) = match text.strip_suffix("-release") {
        Some(id) => (id, false),
        None => (text, true),
    };
    let action = Action::ALL.into_iter().find(|action| action.id() == id)?;
    Some((action, pressed))
}

/// The saved shortcuts, as `Shortcut` text.
pub type Bindings = BTreeMap<Action, String>;

/// Modifiers and one key, saved as text like "Ctrl+Shift+M" with egui's key names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shortcut {
    ctrl: bool,
    alt: bool,
    shift: bool,
    /// Cmd on macOS.
    command: bool,
    key: Key,
}

impl Shortcut {
    /// None for keys global shortcuts can't use (see `names`), and for keys without a
    /// modifier other than F keys: a global M would take that letter from every app.
    pub fn new(modifiers: Modifiers, key: Key) -> Option<Self> {
        let shortcut = Self {
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
            command: modifiers.mac_cmd,
            key,
        };
        let (code, _) = names(key)?;
        let bare = !(shortcut.ctrl || shortcut.alt || shortcut.shift || shortcut.command);
        (!bare || code.starts_with('F')).then_some(shortcut)
    }

    pub fn parse(text: &str) -> Option<Self> {
        let (held, key) = text.rsplit_once('+').unwrap_or(("", text));
        let mut modifiers = Modifiers::NONE;
        for name in held.split('+').filter(|name| !name.is_empty()) {
            match name {
                "Ctrl" => modifiers.ctrl = true,
                "Alt" => modifiers.alt = true,
                "Shift" => modifiers.shift = true,
                "Cmd" => modifiers.mac_cmd = true,
                _ => return None,
            }
        }
        Self::new(modifiers, Key::from_name(key)?)
    }

    fn held(self) -> [(bool, [&'static str; 2]); 4] {
        [
            (self.ctrl, ["Ctrl", "CTRL"]),
            (self.alt, ["Alt", "ALT"]),
            (self.shift, ["Shift", "SHIFT"]),
            (self.command, ["Cmd", "LOGO"]),
        ]
    }

    fn hotkey(self) -> Option<HotKey> {
        let code: Code = names(self.key)?.0.parse().ok()?;
        let mut mods = Mods::empty();
        mods.set(Mods::CONTROL, self.ctrl);
        mods.set(Mods::ALT, self.alt);
        mods.set(Mods::SHIFT, self.shift);
        mods.set(Mods::META, self.command);
        Some(HotKey::new(Some(mods), code))
    }

    /// The preferred trigger for the portal, in the XDG shortcuts format: "CTRL+SHIFT+m".
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    fn trigger(self) -> Option<String> {
        let (_, keysym) = names(self.key)?;
        let mut text: String = (self.held().into_iter())
            .filter(|(on, _)| *on)
            .map(|(_, [_, name])| format!("{name}+"))
            .collect();
        text.push_str(&keysym);
        Some(text)
    }
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, [name, _]) in self.held() {
            if on {
                write!(f, "{name}+")?;
            }
        }
        f.write_str(self.key.name())
    }
}

/// A key's W3C code name (for `global-hotkey`) and X keysym name (for the portal).
/// Only keys that are the same on every layout and platform: letters, digits, F keys,
/// navigation and the common punctuation.
fn names(key: Key) -> Option<(String, String)> {
    let name = key.name();
    match name.as_bytes() {
        [b'A'..=b'Z'] => return Some((format!("Key{name}"), name.to_ascii_lowercase())),
        [b'0'..=b'9'] => return Some((format!("Digit{name}"), name.into())),
        [b'F', ..] if name[1..].parse().is_ok_and(|n: u8| (1..=24).contains(&n)) => {
            return Some((name.into(), name.into()));
        }
        _ => {}
    }
    let (code, keysym) = match key {
        Key::Space => ("Space", "space"),
        Key::Enter => ("Enter", "Return"),
        Key::Tab => ("Tab", "Tab"),
        Key::Backspace => ("Backspace", "BackSpace"),
        Key::Insert => ("Insert", "Insert"),
        Key::Delete => ("Delete", "Delete"),
        Key::Home => ("Home", "Home"),
        Key::End => ("End", "End"),
        Key::PageUp => ("PageUp", "Page_Up"),
        Key::PageDown => ("PageDown", "Page_Down"),
        Key::ArrowUp => ("ArrowUp", "Up"),
        Key::ArrowDown => ("ArrowDown", "Down"),
        Key::ArrowLeft => ("ArrowLeft", "Left"),
        Key::ArrowRight => ("ArrowRight", "Right"),
        Key::Minus => ("Minus", "minus"),
        Key::Equals => ("Equal", "equal"),
        Key::OpenBracket => ("BracketLeft", "bracketleft"),
        Key::CloseBracket => ("BracketRight", "bracketright"),
        Key::Backslash => ("Backslash", "backslash"),
        Key::Semicolon => ("Semicolon", "semicolon"),
        Key::Quote => ("Quote", "apostrophe"),
        Key::Backtick => ("Backquote", "grave"),
        Key::Comma => ("Comma", "comma"),
        Key::Period => ("Period", "period"),
        Key::Slash => ("Slash", "slash"),
        _ => return None,
    };
    Some((code.into(), keysym.into()))
}

/// Actions that share their keys with another action; none of them is bound.
pub fn conflicts(bindings: &Bindings) -> BTreeSet<Action> {
    let parsed: Vec<_> = (bindings.iter())
        .filter_map(|(action, text)| Some((*action, Shortcut::parse(text)?)))
        .collect();
    (parsed.iter())
        .filter(|(a, keys)| parsed.iter().any(|(b, other)| a != b && keys == other))
        .map(|(action, _)| *action)
        .collect()
}

/// Wayland apps can't bind keys themselves; the portal does it.
fn wayland() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Where presses go while the home screen exists. Listeners run on their own threads
/// (or in the main thread's event loop) and outlive any one `Hotkeys`.
struct Route {
    audio: Audio,
    ctx: egui::Context,
    actions: mpsc::Sender<Action>,
    /// `global-hotkey` IDs of the registered shortcuts.
    ids: HashMap<u32, Action>,
}

static ROUTE: Mutex<Option<Route>> = Mutex::new(None);

impl Route {
    /// Push-to-talk goes straight to the microphone: a hidden window may get a frame
    /// only every 250 ms. The rest runs in the next frame.
    fn deliver(&self, action: Action, pressed: bool) {
        if action == Action::PushToTalk {
            self.audio.set_hotkey_down(pressed);
        } else if pressed {
            let _ = self.actions.send(action);
            self.ctx.request_repaint();
        }
    }
}

/// A shortcut from the portal or the command line went down or up.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn deliver(action: Action, pressed: bool) {
    if let Some(route) = &*lock(&ROUTE) {
        route.deliver(action, pressed);
    }
}

fn native_event(event: GlobalHotKeyEvent) {
    if let Some(route) = &*lock(&ROUTE)
        && let Some(&action) = route.ids.get(&event.id)
    {
        route.deliver(action, event.state == HotKeyState::Pressed);
    }
}

enum Backend {
    /// Nothing bound yet.
    Idle,
    Native(GlobalHotKeyManager, Vec<HotKey>),
    #[cfg(target_os = "linux")]
    Portal(portal::Portal),
    Failed,
}

/// What this desktop offers, for Settings.
#[derive(Clone)]
pub enum Support {
    /// RustTalk binds the keys.
    Keys,
    Checking,
    /// The desktop binds them and shows the keys it picked (Wayland only).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Portal {
        /// It can open its own shortcut settings.
        configurable: bool,
        triggers: BTreeMap<Action, String>,
    },
    /// Neither: the person binds the command line in their desktop.
    Unsupported,
}

pub struct Hotkeys {
    ctx: egui::Context,
    actions: mpsc::Receiver<Action>,
    backend: Backend,
    wayland: bool,
    /// What the backend has, so changes apply once.
    applied: Bindings,
    /// Shortcuts the system refused because another app holds them.
    taken: BTreeSet<Action>,
    /// The action whose keys Settings is recording.
    recording: Option<Action>,
    /// "Share the window I'm using" looks the window up off the UI thread.
    window: Option<mpsc::Receiver<anyhow::Result<Option<screen::Target>>>>,
}

impl Hotkeys {
    pub fn new(ctx: &egui::Context, audio: Audio) -> Self {
        let (sender, actions) = mpsc::channel();
        // Tests never listen on this session's keys or socket.
        if !cfg!(test) {
            *lock(&ROUTE) = Some(Route {
                audio,
                ctx: ctx.clone(),
                actions: sender,
                ids: HashMap::new(),
            });
            #[cfg(target_os = "linux")]
            ipc::listen();
        }
        Self {
            ctx: ctx.clone(),
            actions,
            backend: Backend::Idle,
            wayland: wayland(),
            applied: Bindings::new(),
            taken: BTreeSet::new(),
            recording: None,
            window: None,
        }
    }

    /// Start listening the first time something needs it.
    fn start(&mut self) {
        if !matches!(self.backend, Backend::Idle) || cfg!(test) {
            return;
        }
        #[cfg(target_os = "linux")]
        if self.wayland {
            self.backend = Backend::Portal(portal::Portal::start(self.ctx.clone()));
            return;
        }
        static HANDLER: Once = Once::new();
        HANDLER.call_once(|| GlobalHotKeyEvent::set_event_handler(Some(native_event)));
        self.backend = match GlobalHotKeyManager::new() {
            Ok(manager) => Backend::Native(manager, Vec::new()),
            Err(error) => {
                tracing::warn!(%error, "global shortcuts unavailable");
                Backend::Failed
            }
        };
    }

    /// Bind `wanted` in place of what is bound now.
    fn apply(&mut self, wanted: Bindings) {
        if wanted == self.applied {
            return;
        }
        self.start();
        let conflicts = conflicts(&wanted);
        let usable: Vec<_> = (wanted.iter())
            .filter(|(action, _)| !conflicts.contains(action))
            .filter_map(|(action, text)| Some((*action, Shortcut::parse(text)?)))
            .collect();
        self.applied = wanted;
        self.taken.clear();
        match &mut self.backend {
            Backend::Native(manager, registered) => {
                if let Err(error) = manager.unregister_all(registered) {
                    tracing::warn!(%error, "releasing global shortcuts failed");
                }
                registered.clear();
                let mut ids = HashMap::new();
                for (action, shortcut) in usable {
                    let Some(hotkey) = shortcut.hotkey() else {
                        continue;
                    };
                    match manager.register(hotkey) {
                        Ok(()) => {
                            ids.insert(hotkey.id(), action);
                            registered.push(hotkey);
                        }
                        Err(error) => {
                            tracing::info!(%error, action = action.id(), "global shortcut taken");
                            self.taken.insert(action);
                        }
                    }
                }
                if let Some(route) = &mut *lock(&ROUTE) {
                    route.ids = ids;
                    // A held push-to-talk key may have just been unbound.
                    route.audio.set_hotkey_down(false);
                }
            }
            #[cfg(target_os = "linux")]
            Backend::Portal(portal) => portal.bind(
                (usable.into_iter())
                    .filter_map(|(action, shortcut)| Some((action, shortcut.trigger()?)))
                    .collect(),
            ),
            Backend::Idle | Backend::Failed => {}
        }
    }

    fn support(&mut self) -> Support {
        if !self.wayland {
            return Support::Keys;
        }
        self.start();
        match &self.backend {
            #[cfg(target_os = "linux")]
            Backend::Portal(portal) => portal.support(),
            Backend::Failed => Support::Unsupported,
            _ => Support::Checking,
        }
    }

    /// Recording takes the keys out of the system, so the recorder sees them.
    pub fn recording(&self) -> bool {
        self.recording.is_some()
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        if !cfg!(test) {
            *lock(&ROUTE) = None;
        }
    }
}

/// Every pass, also while the window is hidden: keep the shortcuts in step with the
/// settings and run the ones pressed since the last pass.
pub fn frame(home: &mut Home) {
    let hotkeys = &mut home.hotkeys;
    if home.settings != Some(Tab::Shortcuts) {
        hotkeys.recording = None;
    }
    // The portal keeps its keys while recording: binding again would ask the person again.
    let wanted = if hotkeys.recording.is_some() && !hotkeys.wayland {
        Bindings::new()
    } else {
        home.voice.audio.settings().hotkeys
    };
    hotkeys.apply(wanted);
    let pressed: Vec<_> = hotkeys.actions.try_iter().collect();
    for action in pressed {
        run(home, action);
    }
    let found = match home.hotkeys.window.as_ref().map(mpsc::Receiver::try_recv) {
        Some(Ok(found)) => Some(found),
        Some(Err(mpsc::TryRecvError::Disconnected)) => Some(Ok(None)),
        Some(Err(mpsc::TryRecvError::Empty)) | None => None,
    };
    if let Some(found) = found {
        home.hotkeys.window = None;
        share(home, found);
    }
}

/// What a shortcut does; every one plays the sound its button plays.
fn run(home: &mut Home, action: Action) {
    match action {
        Action::ToggleMute => user_bar::toggle_mute(home),
        Action::ToggleDeafen => user_bar::toggle_deafen(home),
        // Already applied as it happened, in `Route::deliver`.
        Action::PushToTalk => {}
        Action::ShareWindow => find_window(home),
        Action::StopSharing => stop_sharing(home),
    }
}

fn toast(home: &mut Home, text: &str) {
    home.toast = Some((text.into(), home.hotkeys.ctx.input(|i| i.time) + 6.0));
}

fn find_window(home: &mut Home) {
    let Some(call) = &home.voice.call else {
        return toast(home, t!("hotkeys.join_first"));
    };
    if !call.can_share_video {
        return toast(home, t!("call.no_video_here"));
    }
    let busy = matches!(home.voice.sharing, Sharing::Starting | Sharing::Changing);
    if busy || home.hotkeys.window.is_some() {
        return;
    }
    let (send, receive) = mpsc::channel();
    let ctx = home.hotkeys.ctx.clone();
    thread::spawn(move || {
        let _ = send.send(screen::focused_window());
        ctx.request_repaint();
    });
    home.hotkeys.window = Some(receive);
}

/// Share the window found, with the last share's quality and stream sound, or switch
/// to it when already sharing.
fn share(home: &mut Home, found: anyhow::Result<Option<screen::Target>>) {
    let target = match found {
        Ok(Some(target)) => target,
        Ok(None) => return toast(home, t!("hotkeys.no_window")),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "finding the window to share failed");
            return toast(home, t!("call.screens_failed"));
        }
    };
    let Some(server) = call::call_server(home) else {
        return;
    };
    let changing = match home.voice.sharing {
        Sharing::On => true,
        Sharing::Off => false,
        Sharing::Starting | Sharing::Changing => return,
    };
    if changing {
        server.send(Command::ChangeScreen(target, false));
    } else {
        // A moderator's mute also takes a stream's sound.
        let sound = if server.voice_muted(&server.dir.me) {
            Vec::new()
        } else {
            home.voice.stream_audio.clone()
        };
        let options = call::screen_options(&home.voice.audio.settings());
        server.send(Command::ShareScreen(target, options, sound));
    }
    home.voice.sharing = if changing {
        Sharing::Changing
    } else {
        Sharing::Starting
    };
}

/// Stop the screen and the sound streamed with it.
fn stop_sharing(home: &mut Home) {
    if home.voice.sharing == Sharing::Off {
        return;
    }
    if let Some(server) = call::call_server(home) {
        server.send(Command::StopScreen);
    }
    home.voice.sharing = Sharing::Off;
    home.voice.stop_preview();
}

/// Settings → Shortcuts.
pub fn settings_page(ui: &mut Ui, home: &mut Home) {
    let mut settings = home.voice.audio.settings();
    let hotkeys = &mut home.hotkeys;
    let support = hotkeys.support();
    ui::page_section(ui, t!("hotkeys.section"));
    ui.weak(t!("hotkeys.intro"));
    match &support {
        Support::Keys => {}
        Support::Checking => {
            ui.spinner();
        }
        Support::Portal { configurable, .. } => {
            ui.weak(t!("hotkeys.portal"));
            #[cfg(target_os = "linux")]
            if *configurable
                && ui::secondary(ui, t!("hotkeys.change_in_system")).clicked()
                && let Backend::Portal(portal) = &hotkeys.backend
            {
                portal.configure();
            }
            #[cfg(not(target_os = "linux"))]
            let _ = configurable;
        }
        Support::Unsupported => {
            ui::error_text(ui, Some(t!("hotkeys.unsupported")));
        }
    }
    ui.add_space(ui::SPACE_S);
    let conflicts = conflicts(&settings.hotkeys);
    for action in Action::ALL {
        if let Support::Unsupported = support {
            ui::setting_row(ui, action.label(), &action.commands(), |_| {});
            continue;
        }
        ui::setting_row(ui, action.label(), action.help(), |ui| {
            recorder(ui, hotkeys, &mut settings.hotkeys, action);
        });
        if hotkeys.recording == Some(action) {
            ui.weak(t!("hotkeys.recording_help"));
        }
        if let Support::Portal { triggers, .. } = &support
            && let Some(keys) = triggers.get(&action).filter(|keys| !keys.is_empty())
        {
            ui.weak(t!("hotkeys.desktop_keys", keys = keys));
        }
        let problem = if conflicts.contains(&action) {
            Some(t!("hotkeys.conflict"))
        } else if hotkeys.taken.contains(&action) {
            Some(t!("hotkeys.taken"))
        } else {
            None
        };
        ui::error_text(ui, problem);
    }
    home.voice.audio.set_settings(settings);
}

/// The keys of one action: a button that records new ones ("Press keys…", Escape
/// cancels) and one that clears them.
fn recorder(ui: &mut Ui, hotkeys: &mut Hotkeys, bindings: &mut Bindings, action: Action) {
    if hotkeys.recording == Some(action) {
        // Taken from the input, so Escape doesn't also close Settings.
        let pressed = ui.input_mut(|input| {
            let pressed = input.events.iter().find_map(|event| match event {
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => Some((*key, *modifiers)),
                _ => None,
            });
            if let Some((key, modifiers)) = pressed {
                input.consume_key(modifiers, key);
            }
            pressed
        });
        match pressed {
            Some((Key::Escape, _)) => hotkeys.recording = None,
            Some((key, modifiers)) => {
                if let Some(shortcut) = Shortcut::new(modifiers, key) {
                    bindings.insert(action, shortcut.to_string());
                    hotkeys.recording = None;
                }
            }
            None => {}
        }
        if ui::primary(ui, t!("hotkeys.press_keys")).clicked() {
            hotkeys.recording = None;
        }
        return;
    }
    let keys = bindings.get(&action);
    if ui::secondary(ui, keys.map_or(t!("hotkeys.not_set"), String::as_str)).clicked() {
        hotkeys.recording = Some(action);
    }
    let size = Vec2::splat(ui::CONTROL_HEIGHT);
    if keys.is_some() && ui::icon_button(ui, "🗑", t!("hotkeys.clear"), Look::Plain, size).clicked()
    {
        bindings.remove(&action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Shortcut {
        Shortcut::parse(text).unwrap()
    }

    #[test]
    fn shortcuts_read_and_write_the_same_text() {
        for text in [
            "Ctrl+Shift+M",
            "Alt+F13",
            "F9",
            "Ctrl+Alt+Shift+Cmd+0",
            "Ctrl+Down",
        ] {
            assert_eq!(keys(text).to_string(), text);
        }
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            Shortcut::new(ctrl, Key::OpenBracket).unwrap().to_string(),
            "Ctrl+OpenBracket"
        );
        // Unknown modifiers and keys, and keys only some layouts have.
        for text in [
            "Hyper+M",
            "Ctrl+Nope",
            "",
            "Ctrl+",
            "Ctrl+Questionmark",
            "Ctrl+Escape",
        ] {
            assert_eq!(Shortcut::parse(text), None, "{text}");
        }
        // A bare letter would take it from every app; bare F keys are fine.
        assert_eq!(Shortcut::parse("M"), None);
        assert_eq!(Shortcut::new(Modifiers::NONE, Key::Space), None);
        assert!(Shortcut::new(Modifiers::NONE, Key::F13).is_some());
        assert_eq!(Shortcut::parse("F25"), None);
    }

    #[test]
    fn shortcuts_turn_into_system_and_portal_keys() {
        let hotkey = keys("Ctrl+Shift+M").hotkey().unwrap();
        assert_eq!(hotkey, "control+shift+KeyM".parse().unwrap());
        assert_eq!(
            keys("Alt+5").hotkey().unwrap(),
            "alt+Digit5".parse().unwrap()
        );
        assert_eq!(
            keys("Cmd+Backtick").hotkey().unwrap(),
            "super+Backquote".parse().unwrap()
        );
        assert_eq!(keys("Ctrl+Shift+M").trigger().unwrap(), "CTRL+SHIFT+m");
        assert_eq!(keys("F13").trigger().unwrap(), "F13");
        assert_eq!(
            keys("Cmd+Alt+PageUp").trigger().unwrap(),
            "ALT+LOGO+Page_Up"
        );
        // Every key the recorder takes has both names.
        for key in Key::ALL {
            if let Some(shortcut) = Shortcut::new(Modifiers::CTRL, *key) {
                assert!(shortcut.hotkey().is_some(), "{key:?}");
                assert!(shortcut.trigger().is_some(), "{key:?}");
            }
        }
    }

    #[test]
    fn shared_keys_are_conflicts() {
        let mut bindings = Bindings::from([
            (Action::ToggleMute, "Ctrl+M".to_owned()),
            (Action::ToggleDeafen, "Ctrl+D".to_owned()),
            (Action::StopSharing, "Ctrl+M".to_owned()),
        ]);
        assert_eq!(
            conflicts(&bindings),
            BTreeSet::from([Action::ToggleMute, Action::StopSharing])
        );
        bindings.insert(Action::StopSharing, "Ctrl+Shift+M".to_owned());
        assert!(conflicts(&bindings).is_empty());
    }

    #[test]
    fn commands_name_an_action_and_a_press() {
        assert_eq!(command("toggle-mute"), Some((Action::ToggleMute, true)));
        assert_eq!(command("push-to-talk"), Some((Action::PushToTalk, true)));
        assert_eq!(
            command("push-to-talk-release"),
            Some((Action::PushToTalk, false))
        );
        assert_eq!(command("share-window"), Some((Action::ShareWindow, true)));
        assert_eq!(command("check-update-now"), None);
        assert_eq!(command(""), None);
        for action in Action::ALL {
            assert_eq!(command(action.id()), Some((action, true)));
        }
    }

    /// A home with one online server, and what it sends to that server.
    fn home() -> (
        Home,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
        tempfile::TempDir,
    ) {
        let ctx = egui::Context::default();
        let (_, identity) = rusttalk_core::identity::DeviceIdentity::create("Desktop").unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut home = Home::new(identity, &ctx, data.path().to_owned()).unwrap();
        let (commands, sent) = tokio::sync::mpsc::unbounded_channel();
        let known = rusttalk_core::auth::KnownServer {
            address: "https://friends.example.org/".parse().unwrap(),
            server_id: [2; 32],
            tls_fingerprint: None,
        };
        let mut server = crate::home::new_server(known, "Friends", commands);
        server.status = crate::server::Status::Online;
        home.servers.push(server);
        (home, sent, data)
    }

    #[test]
    fn shortcuts_do_what_their_buttons_do() {
        let (mut home, mut sent, _data) = home();
        run(&mut home, Action::ShareWindow);
        assert_eq!(
            home.toast.as_ref().map(|(text, _)| text.as_str()),
            Some(t!("hotkeys.join_first"))
        );
        assert!(home.hotkeys.window.is_none());

        home.voice.call = Some(crate::server::Call {
            server: home.servers[0].address.clone(),
            channel: [1; 32],
            state: crate::server::CallState::Connected,
            stats: Default::default(),
            can_talk: true,
            can_share_video: true,
        });
        let audio = home.voice.audio.clone();
        run(&mut home, Action::ToggleMute);
        assert!(audio.muted());
        assert!(matches!(sent.try_recv(), Ok(Command::SyncVoiceStatus)));
        run(&mut home, Action::ToggleDeafen);
        assert!(audio.deafened());
        assert!(matches!(sent.try_recv(), Ok(Command::SyncVoiceStatus)));
        // Unmuting also undeafens, like the mic button.
        run(&mut home, Action::ToggleMute);
        assert!(!audio.muted() && !audio.deafened());
        assert!(matches!(sent.try_recv(), Ok(Command::SyncVoiceStatus)));

        // Linux shares through the desktop's picker, with the last share's options.
        #[cfg(target_os = "linux")]
        {
            let picker = screen::targets().unwrap().remove(0);
            share(&mut home, Ok(Some(picker.clone())));
            let options = call::screen_options(&audio.settings());
            assert!(matches!(
                sent.try_recv(),
                Ok(Command::ShareScreen(target, used, _)) if target == picker && used == options
            ));
            assert!(home.voice.sharing == Sharing::Starting);
            // Starting already: a second press waits.
            share(&mut home, Ok(Some(picker)));
            assert!(sent.try_recv().is_err());
        }

        home.voice.sharing = Sharing::On;
        share(&mut home, Ok(None));
        assert_eq!(
            home.toast.as_ref().map(|(text, _)| text.as_str()),
            Some(t!("hotkeys.no_window"))
        );
        run(&mut home, Action::StopSharing);
        assert!(matches!(sent.try_recv(), Ok(Command::StopScreen)));
        assert!(home.voice.sharing == Sharing::Off);
        run(&mut home, Action::StopSharing);
        assert!(sent.try_recv().is_err());
    }
}
