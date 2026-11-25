//! A diagram drawn together in the call, like a shared terminal: anyone starts one
//! from the call's "…" menu, it opens on the stage for everyone in the call, and others
//! ask to edit it, which its host allows or not. Leaving it returns to the call.

use eframe::egui::{self, Align, Align2, FontId, Layout, Rect, Ui, UiBuilder, vec2};
use rusttalk_core::diagram::{Access, Doc, Feed, REQUEST_TIMEOUT};

use super::call_server;
use crate::{
    diagram::{self as canvas, Canvas},
    home::{Home, face},
    net::{Command, diagram::Guests},
    server::Server,
    ui::{self, Look},
};

/// Allow can't be clicked until the prompt has been up this long, so a click meant for
/// something else can't let someone edit.
const ALLOW_DELAY: f64 = 1.0;
const TOOL: egui::Vec2 = egui::Vec2::splat(30.0);

/// Editing a diagram someone else hosts.
#[derive(Clone, Copy, Default, PartialEq)]
pub enum Editing {
    #[default]
    Idle,
    /// Asked, at this window time.
    Asking(f64),
    On,
}

/// What the "…" menu offers for diagrams.
#[derive(Clone, Copy)]
pub enum Pick {
    Start,
    Open,
    Export,
    End,
    Leave,
}

/// What the diagram's own buttons on the stage asked for.
pub enum TileAct {
    AskToEdit,
    Leave,
    End,
}

/// A file dialog's outcome, from its own thread.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
enum FileDone {
    Opened(anyhow::Result<Doc>),
    Saved(anyhow::Result<()>),
}

#[derive(Default)]
pub struct Diagrams {
    /// We host the session.
    pub hosting: bool,
    /// The host's device (LiveKit identity), while we follow someone else's diagram.
    pub following: Option<String>,
    /// The diagram, once it arrived.
    pub doc: Option<Doc>,
    pub canvas: Canvas,
    pub editing: Editing,
    /// Who asks to edit ours, and who may.
    pub guests: Guests,
    /// The device whose request the prompt shows, since when, and since when the window
    /// had the focus (Allow waits for that).
    prompt: Option<(String, f64, f64)>,
    /// A host whose diagram we left: not joined again on our own while it lasts.
    declined: Option<String>,
    /// The stage presented this session already.
    presented: bool,
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    files: Option<std::sync::mpsc::Receiver<FileDone>>,
}

impl Diagrams {
    /// We host or follow a diagram.
    pub fn open(&self) -> bool {
        self.hosting || self.following.is_some()
    }

    /// We may change the diagram.
    fn editable(&self) -> bool {
        self.hosting || self.editing == Editing::On
    }

    /// Forget the session, keeping a pending file dialog.
    fn close(&mut self) {
        self.hosting = false;
        self.following = None;
        self.doc = None;
        self.canvas = Canvas::default();
        self.editing = Editing::Idle;
        self.guests = Guests::default();
        self.prompt = None;
        self.presented = false;
    }
}

/// Start a diagram session with `doc`, ending any we follow.
pub fn start(home: &mut Home, doc: Doc) {
    let Some(server) = call_server(home) else {
        return;
    };
    server.send(Command::StartDiagram(Box::new(doc.clone())));
    let diagrams = &mut home.voice.diagrams;
    diagrams.close();
    diagrams.hosting = true;
    diagrams.doc = Some(doc);
}

/// End our session for everyone.
pub fn stop(home: &mut Home) {
    if home.voice.diagrams.hosting
        && let Some(server) = call_server(home)
    {
        server.send(Command::StopDiagram);
    }
    home.voice.diagrams.close();
}

/// Follow the diagram `host` (a device) has, instead of any other.
pub fn join(home: &mut Home, host: &str) {
    let diagrams = &home.voice.diagrams;
    if diagrams.hosting || diagrams.following.as_deref() == Some(host) {
        return;
    }
    home.voice.diagrams.close();
    home.voice.diagrams.following = Some(host.to_owned());
    home.voice.diagrams.declined = None;
    if let Some(server) = call_server(home) {
        server.send(Command::JoinDiagram(Some(host.to_owned())));
    }
}

/// Stop following a diagram; back to the call.
pub fn leave(home: &mut Home) {
    let Some(host) = home.voice.diagrams.following.take() else {
        return;
    };
    home.voice.diagrams.close();
    home.voice.diagrams.declined = Some(host);
    if let Some(server) = call_server(home) {
        server.send(Command::JoinDiagram(None));
    }
}

/// Ask to edit the diagram we follow, or stop asking or editing.
pub fn ask(home: &mut Home, now: f64) {
    let diagrams = &mut home.voice.diagrams;
    let command = if diagrams.editing == Editing::Idle {
        diagrams.editing = Editing::Asking(now);
        Command::AskToEdit
    } else {
        diagrams.editing = Editing::Idle;
        diagrams.canvas.reset();
        Command::StopEditing
    };
    if let Some(server) = call_server(home) {
        server.send(command);
    }
}

/// The name of whoever hosts the diagram we follow.
fn host_name(home: &Home) -> Option<String> {
    let host = home.voice.diagrams.following.as_deref()?;
    let server = call_server(home)?;
    let call = home.voice.call.as_ref()?;
    let peer = (server.voice.get(&call.channel)?.iter())
        .find(|peer| peer.diagram.as_deref() == Some(host))?;
    Some(peer.name(server))
}

/// The session ended without us ending it: say so, and go back to the call.
fn ended(home: &mut Home, now: f64) {
    if !home.voice.diagrams.open() {
        return;
    }
    let note = match host_name(home) {
        Some(name) if !home.voice.diagrams.hosting => t!("diagram.ended_by", name = name),
        _ => t!("diagram.ended").into(),
    };
    // Its announcement may linger a moment: don't join it again meanwhile.
    let host = home.voice.diagrams.following.take();
    home.voice.diagrams.close();
    home.voice.diagrams.declined = host;
    home.voice.stage.unpresent_diagram();
    home.toast = Some((note, now + 6.0));
}

/// What the call says about the diagram we host or follow.
pub fn feed(home: &mut Home, feed: Feed, now: f64) {
    match feed {
        Feed::Reset(doc) => {
            let diagrams = &mut home.voice.diagrams;
            if !diagrams.open() {
                return;
            }
            diagrams.doc = Some(doc);
            if !diagrams.presented {
                // A new session takes the stage, for everyone in the call.
                diagrams.presented = true;
                diagrams.canvas.refit();
                home.voice.stage.present();
            }
        }
        Feed::Ops(ops) => {
            if let Some(doc) = &mut home.voice.diagrams.doc {
                for op in &ops {
                    doc.apply(op);
                }
            }
        }
        Feed::Ended => ended(home, now),
        Feed::Editing(access) => answered(home, access, now),
        Feed::Nothing | Feed::Resync => {}
    }
}

/// Our session couldn't start, or the call's connection dropped it.
pub fn stopped(home: &mut Home) {
    if home.voice.diagrams.hosting {
        home.voice.diagrams.close();
    }
}

/// What the host said about editing: a note unless it was a yes.
fn answered(home: &mut Home, access: Access, now: f64) {
    let name = host_name(home).unwrap_or_else(|| t!("remote.the_sharer").into());
    let diagrams = &mut home.voice.diagrams;
    let note = match access {
        Access::Granted if matches!(diagrams.editing, Editing::Asking(_)) => {
            diagrams.editing = Editing::On;
            None
        }
        Access::Granted => None,
        Access::Denied => Some(t!("diagram.denied", name = name)),
        Access::Revoked => Some(t!("diagram.taken_back", name = name)),
    };
    if let Some(note) = note {
        diagrams.editing = Editing::Idle;
        diagrams.canvas.reset();
        home.toast = Some((note, now + 6.0));
    }
}

/// Once per frame: notice a session that is gone, join a new one on our own, give up
/// asking to edit after no answer, and take in what a file dialog gave.
pub fn frame(home: &mut Home, now: f64) {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    files(home, now);
    if home.voice.call.is_none() {
        home.voice.diagrams.close();
        home.voice.diagrams.declined = None;
        return;
    }
    // A little longer than the host waits, so their answer can still arrive.
    if let Editing::Asking(since) = home.voice.diagrams.editing
        && now - since > REQUEST_TIMEOUT.as_secs_f64() + 2.0
    {
        let name = host_name(home).unwrap_or_else(|| t!("remote.they").into());
        home.toast = Some((t!("remote.no_answer", name = name), now + 6.0));
        ask(home, now);
    }
    let Some(hosts) = call_server(home).and_then(|server| {
        let call = home.voice.call.as_ref()?;
        let peers = server.voice.get(&call.channel)?;
        let others = peers.iter().filter(|peer| peer.person != server.dir.me);
        Some(
            others
                .filter_map(|peer| peer.diagram.clone())
                .collect::<Vec<_>>(),
        )
    }) else {
        return;
    };
    let diagrams = &mut home.voice.diagrams;
    if (diagrams.declined.as_ref()).is_some_and(|host| !hosts.contains(host)) {
        diagrams.declined = None;
    }
    if let Some(host) = &diagrams.following
        && !hosts.contains(host)
    {
        ended(home, now);
        return;
    }
    if !diagrams.open()
        && let Some(host) = hosts
            .iter()
            .find(|host| diagrams.declined.as_ref() != Some(*host))
    {
        let host = host.clone();
        join(home, &host);
    }
}

/// The diagram rows of the call's "…" menu.
pub fn menu_rows(m: &mut ui::Menu, diagrams: &Diagrams) -> Option<Pick> {
    let mut pick = None;
    let files = cfg!(not(any(target_os = "android", target_os = "ios")));
    if diagrams.open() {
        if files && diagrams.doc.is_some() && m.item("💾", t!("diagram.export")).clicked() {
            pick = Some(Pick::Export);
        }
        if diagrams.hosting {
            if m.danger("⏹", t!("diagram.end")).clicked() {
                pick = Some(Pick::End);
            }
        } else if m.item("⏹", t!("diagram.leave")).clicked() {
            pick = Some(Pick::Leave);
        }
        return pick;
    }
    if m.item("📊", t!("diagram.start")).clicked() {
        pick = Some(Pick::Start);
    }
    if files && m.item("🗀", t!("diagram.open_file")).clicked() {
        pick = Some(Pick::Open);
    }
    pick
}

/// Carry out what the "…" menu picked.
pub fn apply(ctx: &egui::Context, home: &mut Home, pick: Pick) {
    match pick {
        Pick::Start => start(home, Doc::default()),
        Pick::End => {
            home.voice.stage.unpresent_diagram();
            stop(home);
        }
        Pick::Leave => {
            home.voice.stage.unpresent_diagram();
            leave(home);
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Pick::Open => open_file(ctx, home),
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        Pick::Export => export_file(ctx, home),
        #[cfg(any(target_os = "android", target_os = "ios"))]
        Pick::Open | Pick::Export => {
            let _ = ctx;
        }
    }
}

/// Pick a draw.io file to start a session with, on its own thread.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn open_file(ctx: &egui::Context, home: &mut Home) {
    let (send, receive) = std::sync::mpsc::channel();
    home.voice.diagrams.files = Some(receive);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("draw.io", &["drawio", "xml"])
            .pick_file()
        else {
            return;
        };
        let read = || -> anyhow::Result<Doc> {
            use anyhow::Context;
            let size = std::fs::metadata(&path)?.len();
            anyhow::ensure!(size <= canvas::drawio::MAX_FILE, "file too big");
            let text = std::fs::read_to_string(&path).context("reading the file")?;
            canvas::drawio::import(&text)
        };
        let _ = send.send(FileDone::Opened(read()));
        ctx.request_repaint();
    });
}

/// Save the diagram as a draw.io file, on its own thread.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn export_file(ctx: &egui::Context, home: &mut Home) {
    let Some(doc) = &home.voice.diagrams.doc else {
        return;
    };
    let file = canvas::drawio::export(doc);
    let (send, receive) = std::sync::mpsc::channel();
    home.voice.diagrams.files = Some(receive);
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("draw.io", &["drawio"])
            .set_file_name("diagram.drawio")
            .save_file()
        else {
            return;
        };
        let saved = std::fs::write(&path, file).map_err(anyhow::Error::from);
        let _ = send.send(FileDone::Saved(saved));
        ctx.request_repaint();
    });
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn files(home: &mut Home, now: f64) {
    let Some(done) = (home.voice.diagrams.files.as_ref()).and_then(|files| files.try_recv().ok())
    else {
        return;
    };
    home.voice.diagrams.files = None;
    let note = match done {
        FileDone::Opened(Ok(doc)) => {
            start(home, doc);
            None
        }
        FileDone::Opened(Err(error)) => {
            tracing::info!(error = %format_args!("{error:#}"), "opening a diagram file failed");
            Some(t!("diagram.open_failed"))
        }
        FileDone::Saved(Ok(())) => Some(t!("diagram.saved")),
        FileDone::Saved(Err(error)) => {
            tracing::warn!(error = %format_args!("{error:#}"), "saving a diagram file failed");
            Some(t!("diagram.save_failed"))
        }
    };
    if let Some(note) = note {
        home.toast = Some((note.into(), now + 6.0));
    }
}

/// "Ada wants to edit your diagram", for each request in turn. Draw before the panels.
pub fn prompt(ctx: &egui::Context, home: &mut Home) {
    let Some(server) = home
        .voice
        .call
        .as_ref()
        .and_then(|call| (home.servers.iter()).find(|s| s.address == call.server))
    else {
        return;
    };
    let now = ctx.input(|input| input.time);
    let diagrams = &mut home.voice.diagrams;
    let first = diagrams.guests.asking.first().cloned();
    let Some((device, person)) = first else {
        diagrams.prompt = None;
        return;
    };
    if (diagrams.prompt.as_ref()).is_none_or(|(shown, ..)| *shown != device) {
        diagrams.prompt = Some((device.clone(), now, now));
        let attention = egui::UserAttentionType::Informational;
        ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(attention));
    }
    let focused = ctx.input(|input| input.viewport().focused).unwrap_or(true);
    let Some((_, shown, ready_from)) = &mut diagrams.prompt else {
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
        let title = t!("diagram.request_title", name = name);
        let (picked, closed) = ui::dialog(ctx, "diagram editing", &title, |ui| {
            // Their picture too, to tell apart people with the same name.
            ui.horizontal(|ui| {
                face(ui, &server.dir, &person, &name, 32.0, false);
                ui.add(egui::Label::new(t!("diagram.allow_help")).wrap());
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
        diagrams.guests.asking.retain(|(asker, _)| *asker != device);
        diagrams.prompt = None;
        server.send(Command::AnswerEdit(device, allow));
    }
}

/// "Ada can edit" for each person allowed to, along the bottom right of our diagram;
/// clicking one takes it back.
fn editors(ui: &mut Ui, rect: Rect, guests: &Guests, server: &Server) {
    if guests.editing.is_empty() {
        return;
    }
    let area = Rect::from_min_max(
        rect.left_bottom() + vec2(ui::SPACE_S, -28.0 - ui::SPACE_S),
        rect.right_bottom() - vec2(ui::SPACE_S, ui::SPACE_S),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(area)
            .layout(Layout::right_to_left(Align::Center)),
    );
    for (device, person) in &guests.editing {
        let text = format!(
            "✏ {}",
            t!("diagram.can_edit", name = server.member_name(person))
        );
        let fill = ui::accent(&child).base;
        let chip = ui::badge(&mut child, &text, fill)
            .interact(egui::Sense::click())
            .on_hover_text(t!("terminal.take_back"));
        if chip.clicked() {
            server.send(Command::RevokeEdit(device.clone()));
        }
    }
}

/// The diagram we host or follow, in its tile: the canvas, its tools, and asking to
/// edit, leaving or ending. While squeezed into the strip (not `active`) it is only
/// shown.
pub fn tile(
    ui: &mut Ui,
    rect: Rect,
    diagrams: &mut Diagrams,
    server: &Server,
    active: bool,
    label: &str,
) -> Option<TileAct> {
    let editable = diagrams.editable();
    let Some(doc) = &mut diagrams.doc else {
        let c = ui::colors(ui);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            t!("diagram.loading"),
            FontId::proportional(ui::TEXT_SMALL),
            c.muted,
        );
        return None;
    };
    let (hosting, editing) = (diagrams.hosting, diagrams.editing);
    let mut act = None;
    let chrome = |ui: &mut Ui| {
        let on = Look::On(ui::accent(ui).base);
        if hosting {
            if ui::icon_button(ui, "⏹", t!("diagram.end"), Look::Danger, TOOL).clicked() {
                act = Some(TileAct::End);
            }
            return;
        }
        let (tip, look) = match editing {
            Editing::Idle => (t!("diagram.ask_to_edit"), Look::Ghost),
            Editing::Asking(_) => (t!("call.cancel_request"), on),
            Editing::On => (t!("diagram.stop_editing"), on),
        };
        if ui::icon_button(ui, "✏", tip, look, TOOL).clicked() {
            act = Some(TileAct::AskToEdit);
        }
        if ui::icon_button(ui, "⏹", t!("diagram.leave"), Look::Ghost, TOOL).clicked() {
            act = Some(TileAct::Leave);
        }
    };
    let ops = canvas::show(
        ui,
        rect,
        doc,
        &mut diagrams.canvas,
        (active, editable),
        label,
        chrome,
    );
    if !ops.is_empty() {
        server.send(Command::EditDiagram(ops));
    }
    if hosting {
        editors(ui, rect, &diagrams.guests, server);
    }
    act
}
