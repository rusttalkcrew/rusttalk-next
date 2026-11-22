//! A shared terminal in the window: sharing ours (with a warning the first time), our
//! own tile, which we type in, and the tile of a terminal we watch.

use eframe::egui::{self, Id as UiId, Rect, Ui};
use rusttalk_core::terminal::Feed;

use super::call_server;
use crate::{
    home::Home,
    net::Command,
    terminal::{self as term, Emulator, Local, Shell},
    ui,
};

/// The size a new share starts at, until its tile is drawn.
const START: (u16, u16) = (100, 30);

#[derive(Default)]
pub struct Terminals {
    /// Ours, while shared; the shell ends when this is dropped.
    pub local: Option<Local>,
    /// The one we watch: the sharer's device (LiveKit identity), and the screen once it
    /// arrived.
    pub watching: Option<(String, Option<Emulator>)>,
    /// The first-time warning is open, to start this shell.
    confirm: Option<Shell>,
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
            Feed::Ended => self.watching = None,
            Feed::Nothing | Feed::Resync => {}
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
    if let Some(server) = call_server(home) {
        server.send(Command::WatchTerminal(Some(sharer.to_owned())));
    }
}

pub fn stop_watching(home: &mut Home) {
    if home.voice.terminals.watching.take().is_some()
        && let Some(server) = call_server(home)
    {
        server.send(Command::WatchTerminal(None));
    }
}

/// Once per frame: stop sharing when the shell ended, and stop watching a terminal that
/// is no longer shared.
pub fn frame(home: &mut Home) {
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

/// Our terminal in its tile: we type in it, and while it isn't squeezed into the strip
/// (`active`) the shell takes the tile's size.
pub fn ours(ui: &mut Ui, rect: Rect, local: &mut Local, active: bool, label: &str) {
    let now = ui.input(|input| input.time);
    if active && local.follow(term::fit(ui, rect), now) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
    }
    let id = UiId::unique("own terminal");
    let bytes = {
        let mut inner = local.shared.lock();
        term::show(ui, rect, id, &mut inner.emulator, active, true, label).1
    };
    if !bytes.is_empty() {
        local.shared.write(&bytes);
    }
}

/// A terminal we watch, in its tile: read only, but it can be scrolled, selected and
/// copied from. `None` until its screen arrived.
pub fn theirs(ui: &mut Ui, rect: Rect, screen: Option<&mut Emulator>, active: bool, label: &str) {
    match screen {
        Some(screen) => {
            let id = UiId::unique(("terminal", label));
            term::show(ui, rect, id, screen, active, false, label);
        }
        None => term::placeholder(ui, rect, t!("call.waiting_picture")),
    }
}
