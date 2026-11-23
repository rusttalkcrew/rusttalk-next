//! A shared terminal in the window: sharing ours (with a warning the first time), our
//! own tile, which we type in, and the tile of a terminal we watch. A viewer can ask
//! to type too; the sharer allows each person and can take it back.

use eframe::egui::{self, Align, Id as UiId, Layout, Rect, Ui, UiBuilder, vec2};
use rusttalk_core::terminal::{Access, Feed, MAX_PASTE, REQUEST_TIMEOUT};

use super::call_server;
use crate::{
    home::{Home, face},
    net::{Command, terminal::Guests},
    server::Server,
    terminal::{self as term, Emulator, Local, Shell},
    ui,
};

/// The size a new share starts at, until its tile is drawn.
const START: (u16, u16) = (100, 30);
/// Allow can't be clicked until the prompt has been up this long, so a click meant for
/// something else can't let someone type.
const ALLOW_DELAY: f64 = 1.0;
/// The height of the "Ada can type" band above our terminal.
const BAND: f32 = 28.0;

/// Typing in a terminal we watch.
#[derive(Clone, Copy, Default, PartialEq)]
pub enum Typing {
    #[default]
    Idle,
    /// Asked, at this window time.
    Asking(f64),
    On,
}

#[derive(Default)]
pub struct Terminals {
    /// Ours, while shared; the shell ends when this is dropped.
    pub local: Option<Local>,
    /// The one we watch: the sharer's device (LiveKit identity), and the screen once it
    /// arrived.
    pub watching: Option<(String, Option<Emulator>)>,
    /// The first-time warning is open, to start this shell.
    confirm: Option<Shell>,
    /// Who asks to type in ours, and who may.
    pub guests: Guests,
    /// The device whose request the prompt shows, since when, and since when the window
    /// had the focus (Allow waits for that).
    prompt: Option<(String, f64, f64)>,
    /// Typing in the one we watch.
    pub typing: Typing,
}

impl Terminals {
    /// What the call says about the terminal we watch.
    pub fn feed(&mut self, feed: Feed) {
        let Some((_, screen)) = &mut self.watching else {
            return;
        };
        match feed {
            Feed::Reset { cols, rows, data } => {
                let mut emulator = Emulator::new(cols, rows);
                emulator.feed(&data);
                *screen = Some(emulator);
            }
            Feed::Bytes(bytes) => {
                if let Some(screen) = screen {
                    screen.feed(&bytes);
                }
            }
            Feed::Resize { cols, rows } => {
                if let Some(screen) = screen {
                    screen.resize(cols, rows);
                }
            }
            Feed::Ended => {
                self.watching = None;
                self.typing = Typing::Idle;
            }
            Feed::Nothing | Feed::Resync | Feed::Typing(_) => {}
        }
    }

    /// The device whose terminal we watch.
    pub fn watched(&self) -> Option<&str> {
        self.watching.as_ref().map(|(sharer, _)| sharer.as_str())
    }
}

/// The share button: start sharing `shell` (after the warning, the first time), or stop.
pub fn toggle(ctx: &egui::Context, home: &mut Home, shell: Shell) {
    if home.voice.terminals.local.is_some() {
        stop(home);
    } else if home.voice.audio.settings().terminal_warned {
        start(ctx, home, shell);
    } else {
        home.voice.terminals.confirm = Some(shell);
    }
}

fn start(ctx: &egui::Context, home: &mut Home, shell: Shell) {
    if !home.voice.audio.can_host_terminal() {
        return;
    }
    let Some(server) = call_server(home) else {
        return;
    };
    let repaint = ctx.clone();
    match Local::start(shell, START.0, START.1, move || repaint.request_repaint()) {
        Ok(local) => {
            server.send(Command::ShareTerminal(local.shared.clone()));
            home.voice.terminals.local = Some(local);
        }
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "starting a shell to share failed");
            let now = ctx.input(|input| input.time);
            home.toast = Some((t!("terminal.failed").into(), now + 6.0));
        }
    }
}

/// Stop sharing our terminal, ending its shell.
pub fn stop(home: &mut Home) {
    home.voice.terminals.confirm = None;
    // Asking and typing end with the share.
    home.voice.terminals.guests = Guests::default();
    home.voice.terminals.prompt = None;
    if home.voice.terminals.local.take().is_some()
        && let Some(server) = call_server(home)
    {
        server.send(Command::StopTerminal);
    }
}

/// Watch the terminal `sharer` (a device) shares, instead of any other.
pub fn watch(home: &mut Home, sharer: &str) {
    if home.voice.terminals.watched() == Some(sharer) {
        return;
    }
    home.voice.terminals.watching = Some((sharer.to_owned(), None));
    home.voice.terminals.typing = Typing::Idle;
    if let Some(server) = call_server(home) {
        server.send(Command::WatchTerminal(Some(sharer.to_owned())));
    }
}

pub fn stop_watching(home: &mut Home) {
    home.voice.terminals.typing = Typing::Idle;
    if home.voice.terminals.watching.take().is_some()
        && let Some(server) = call_server(home)
    {
        server.send(Command::WatchTerminal(None));
    }
}

/// Ask to type in the terminal we watch, or stop asking or typing.
pub fn ask(home: &mut Home, now: f64) {
    let terms = &mut home.voice.terminals;
    let command = if terms.typing == Typing::Idle {
        terms.typing = Typing::Asking(now);
        Command::AskToType
    } else {
        terms.typing = Typing::Idle;
        Command::StopTyping
    };
    if let Some(server) = call_server(home) {
        server.send(command);
    }
}

/// What the sharer said about typing: a note unless it was a yes.
pub fn answered(home: &mut Home, access: Access, now: f64) {
    let name = sharer_name(home).unwrap_or_else(|| t!("remote.the_sharer").into());
    let terms = &mut home.voice.terminals;
    let note = match access {
        Access::Granted if matches!(terms.typing, Typing::Asking(_)) => {
            terms.typing = Typing::On;
            None
        }
        Access::Granted => None,
        Access::Denied => Some(t!("terminal.denied", name = name)),
        Access::Revoked => Some(t!("terminal.taken_back", name = name)),
    };
    if let Some(note) = note {
        terms.typing = Typing::Idle;
        home.toast = Some((note, now + 6.0));
    }
}

/// The name of whoever shares the terminal we watch.
fn sharer_name(home: &Home) -> Option<String> {
    let sharer = home.voice.terminals.watched()?;
    let server = call_server(home)?;
    let call = home.voice.call.as_ref()?;
    let peer = (server.voice.get(&call.channel)?.iter())
        .find(|peer| peer.terminal.as_deref() == Some(sharer))?;
    Some(peer.name(server))
}

/// Once per frame: stop sharing when the shell ended, stop watching a terminal that is
/// no longer shared, and give up asking to type after no answer.
pub fn frame(home: &mut Home, now: f64) {
    // A little longer than the sharer waits, so their answer can still arrive.
    if let Typing::Asking(since) = home.voice.terminals.typing
        && now - since > REQUEST_TIMEOUT.as_secs_f64() + 2.0
    {
        let name = sharer_name(home).unwrap_or_else(|| t!("remote.they").into());
        home.toast = Some((t!("remote.no_answer", name = name), now + 6.0));
        ask(home, now);
    }
    if home
        .voice
        .terminals
        .local
        .as_ref()
        .is_some_and(Local::exited)
    {
        stop(home);
    }
    if let Some(sharer) = home.voice.terminals.watched() {
        let shared = call_server(home)
            .zip(home.voice.call.as_ref())
            .and_then(|(server, call)| server.voice.get(&call.channel))
            .is_some_and(|peers| {
                (peers.iter()).any(|peer| peer.terminal.as_deref() == Some(sharer))
            });
        if !shared {
            home.voice.terminals.watching = None;
        }
    }
}

/// The warning before sharing a terminal for the first time.
pub fn dialog(ctx: &egui::Context, home: &mut Home) {
    let Some(shell) = home.voice.terminals.confirm else {
        return;
    };
    let (answer, closed) = ui::dialog(ctx, "share terminal", t!("terminal.warn_title"), |ui| {
        ui.add(egui::Label::new(t!("terminal.warn_text")).wrap());
        let mut answer = None;
        ui::footer(ui, |ui| {
            if ui::primary(ui, t!("terminal.share")).clicked() {
                answer = Some(true);
            }
            if ui::secondary(ui, t!("common.cancel")).clicked() {
                answer = Some(false);
            }
        });
        answer
    });
    let Some(share) = answer.or(closed.then_some(false)) else {
        return;
    };
    home.voice.terminals.confirm = None;
    if share {
        let mut settings = home.voice.audio.settings();
        settings.terminal_warned = true;
        home.voice.audio.set_settings(settings);
        start(ctx, home, shell);
    }
}

/// "Ada wants to type in your terminal", for each request in turn. Draw before the panels.
pub fn prompt(ctx: &egui::Context, home: &mut Home) {
    // Field by field, so the terminals stay writable.
    let Some(server) = home
        .voice
        .call
        .as_ref()
        .and_then(|call| (home.servers.iter()).find(|s| s.address == call.server))
    else {
        return;
    };
    let now = ctx.input(|input| input.time);
    let terms = &mut home.voice.terminals;
    let first = terms.guests.asking.first().cloned();
    let Some((device, person)) = first else {
        terms.prompt = None;
        return;
    };
    if terms
        .prompt
        .as_ref()
        .is_none_or(|(shown, ..)| *shown != device)
    {
        terms.prompt = Some((device.clone(), now, now));
        // The person may be in another window: show them something is waiting.
        let attention = egui::UserAttentionType::Informational;
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(attention));
    }
    let focused = ctx.input(|input| input.viewport().focused).unwrap_or(true);
    let Some((_, shown, ready_from)) = &mut terms.prompt else {
        return;
    };
    // The wait before Allow starts over while the window isn't in front.
    if !focused {
        *ready_from = now;
    }
    let (shown, ready_from) = (*shown, *ready_from);
    let answer = if now - shown > REQUEST_TIMEOUT.as_secs_f64() {
        Some(false)
    } else {
        let ready = now - ready_from >= ALLOW_DELAY;
        if !ready {
            ctx.request_repaint();
        }
        let name = server.member_name(&person);
        let title = t!("terminal.request_title", name = name);
        let (picked, closed) = ui::dialog(ctx, "terminal typing", &title, |ui| {
            // Their picture too, to tell apart people with the same name.
            ui.horizontal(|ui| {
                face(ui, &server.dir, &person, &name, 32.0, false);
                ui.add(egui::Label::new(t!("terminal.allow_help")).wrap());
            });
            let mut picked = None;
            ui::footer(ui, |ui| {
                let allow = ui.add_enabled_ui(ready, |ui| ui::primary(ui, t!("remote.allow")));
                if allow.inner.clicked() {
                    picked = Some(true);
                }
                if ui::secondary(ui, t!("remote.deny")).clicked() {
                    picked = Some(false);
                }
            });
            picked
        });
        // Closing the prompt any other way is a no.
        picked.or(closed.then_some(false))
    };
    if let Some(allow) = answer {
        terms.guests.asking.retain(|(asker, _)| *asker != device);
        terms.prompt = None;
        server.send(Command::AnswerTyping(device, allow));
    }
}

/// "Ada can type" for each person allowed to, at the top of our terminal; clicking one
/// takes it back.
fn typists(ui: &mut Ui, rect: Rect, guests: &Guests, server: Option<&Server>) {
    let Some(server) = server.filter(|_| !guests.typing.is_empty()) else {
        return;
    };
    let area = Rect::from_min_size(
        rect.min + vec2(ui::SPACE_S, ui::SPACE_S),
        vec2(rect.width() - 2.0 * ui::SPACE_S, BAND - ui::SPACE_S),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(area)
            .layout(Layout::left_to_right(Align::Center)),
    );
    for (device, person) in &guests.typing {
        let text = format!(
            "⌨ {}",
            t!("terminal.can_type", name = server.member_name(person))
        );
        let fill = ui::accent(&child).base;
        let chip = ui::badge(&mut child, &text, fill)
            .interact(egui::Sense::click())
            .on_hover_text(t!("terminal.take_back"));
        if chip.clicked() {
            server.send(Command::RevokeTyping(device.clone()));
        }
    }
}

/// Our terminal in its tile: we type in it, and while it isn't squeezed into the strip
/// (`active`) the shell takes the tile's size. Shows who else may type.
pub fn ours(
    ui: &mut Ui,
    rect: Rect,
    local: &mut Local,
    (guests, server): (&Guests, Option<&Server>),
    active: bool,
    label: &str,
) {
    // Who may type gets a band above the screen, rather than covering it.
    let mut screen = rect;
    if !guests.typing.is_empty() {
        screen.min.y += BAND;
    }
    let now = ui.input(|input| input.time);
    if active && local.follow(term::fit(ui, screen), now) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
    let id = UiId::unique("own terminal");
    let bytes = {
        let mut inner = local.shared.lock();
        let typing = Some(usize::MAX);
        term::show(ui, screen, id, &mut inner.emulator, active, typing, label).bytes
    };
    if !bytes.is_empty() {
        local.shared.write(&bytes);
    }
    typists(ui, rect, guests, server);
}

/// A terminal we watch, in its tile: it can be scrolled, selected and copied from, and
/// typed in once the sharer allowed it. `None` until its screen arrived. Returns what
/// was typed, and whether a paste was too long to send.
pub fn theirs(
    ui: &mut Ui,
    rect: Rect,
    screen: Option<&mut Emulator>,
    (active, typing): (bool, bool),
    label: &str,
) -> (Vec<u8>, bool) {
    match screen {
        Some(screen) => {
            let id = UiId::unique(("terminal", label));
            // The sharer would drop a longer paste part way; refuse it here instead.
            let typing = typing.then_some(MAX_PASTE);
            let shown = term::show(ui, rect, id, screen, active, typing, label);
            (shown.bytes, shown.refused)
        }
        None => {
            term::placeholder(ui, rect, t!("call.waiting_picture"));
            (Vec::new(), false)
        }
    }
}

/// The note when a paste is too long to type in someone's terminal.
pub fn paste_refused(home: &mut Home, now: f64) {
    home.toast = Some((t!("terminal.paste_too_long").into(), now + 6.0));
}

/// Send what was typed into the terminal we watch.
pub fn send_typed(home: &Home, bytes: Vec<u8>) {
    if !bytes.is_empty()
        && let Some(server) = call_server(home)
    {
        server.send(Command::TypeInTerminal(bytes));
    }
}
