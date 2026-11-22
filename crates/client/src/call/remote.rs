//! Remote control of a shared screen in the window: the prompt and the "Stop remote
//! control" bar for the sharer, and for the viewer, the input sent from the screen's tile.

use std::collections::BTreeSet;

use eframe::egui::{
    self, Align, Event, EventFilter, Frame, Key, Layout, Margin, MouseWheelUnit, PointerButton,
    Pos2, Rect, Response, Ui,
};
use rusttalk_core::control::{Button, Input, REQUEST_TIMEOUT, Refusal, scan_code};

use super::call_server;
use crate::{
    home::Home,
    net::{Command, ControlView},
    server::Server,
    ui,
};

/// Allow can't be clicked until the prompt has been up this long, so a click meant for
/// something else can't grant control.
const ALLOW_DELAY: f64 = 1.0;
/// Holding Escape this long stops controlling.
const ESCAPE_HOLD: f64 = 1.0;
/// Pixels per wheel notch for smooth (touchpad) scrolling.
const POINTS_PER_NOTCH: f32 = 40.0;

/// Remote control as the window sees it.
#[derive(Default)]
pub struct Control {
    pub view: ControlView,
    /// When the prompt appeared, and when we asked, in window time.
    asked_at: f64,
    asking_at: f64,
    /// What the viewer holds down on the sharer's side, to let go of it.
    keys: BTreeSet<String>,
    buttons: BTreeSet<Button>,
    escape_since: Option<f64>,
}

impl Control {
    pub fn update(&mut self, view: ControlView, now: f64) {
        if view.asked.is_some() && view.asked != self.view.asked {
            self.asked_at = now;
        }
        if matches!(&view.screen, Some((_, false))) && view.screen != self.view.screen {
            self.asking_at = now;
        }
        if !matches!(view.screen, Some((_, true))) {
            self.keys.clear();
            self.buttons.clear();
            self.escape_since = None;
        }
        self.view = view;
    }

    /// The screen (track ID) we control or ask to control, and whether we control it.
    pub fn screen(&self, sid: &str) -> Option<bool> {
        let (screen, on) = self.view.screen.as_ref()?;
        (screen == sid).then_some(*on)
    }

    /// Let go of everything held on the sharer's side.
    fn release(&mut self) -> Vec<Input> {
        let keys = std::mem::take(&mut self.keys);
        let buttons = std::mem::take(&mut self.buttons);
        let keys = keys.into_iter().map(|key| Input::Key { key, down: false });
        let buttons = buttons.into_iter().map(|button| Input::Button {
            button,
            down: false,
            x: 0.0,
            y: 0.0,
        });
        keys.chain(buttons).collect()
    }

    /// This frame's input over `picture`, the shared screen as shown, from the tile's
    /// `response`. Only while the tile has the keyboard focus; a click gives it focus.
    /// Returns the input to send, and whether Escape was held long enough to stop.
    pub fn capture(&mut self, ui: &Ui, response: &Response, picture: Rect) -> (Vec<Input>, bool) {
        if response.clicked() && !response.has_focus() {
            response.request_focus();
            return (Vec::new(), false);
        }
        if !response.has_focus() {
            self.escape_since = None;
            return (self.release(), false);
        }
        // Tab, arrows and Escape go to the shared screen, not to moving the focus.
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            );
        });
        let at = |pos: Pos2| {
            let point = (pos - picture.min) / picture.size();
            (point.x.clamp(0.0, 1.0), point.y.clamp(0.0, 1.0))
        };
        let over = response.hovered();
        let mut moved = None;
        let mut sent = Vec::new();
        let events = ui.input(|input| input.events.clone());
        for event in events {
            match event {
                Event::PointerMoved(pos) if over && picture.contains(pos) => moved = Some(at(pos)),
                Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } => {
                    let button = match button {
                        PointerButton::Primary => Button::Left,
                        PointerButton::Secondary => Button::Right,
                        PointerButton::Middle => Button::Middle,
                        _ => continue,
                    };
                    let (x, y) = at(pos);
                    if pressed && over && picture.contains(pos) {
                        self.buttons.insert(button);
                    } else if pressed || !self.buttons.remove(&button) {
                        continue;
                    }
                    sent.push(Input::Button {
                        button,
                        down: pressed,
                        x,
                        y,
                    });
                }
                Event::MouseWheel { unit, delta, .. } if over => {
                    let notches = match unit {
                        MouseWheelUnit::Point => delta / POINTS_PER_NOTCH,
                        MouseWheelUnit::Line => delta,
                        MouseWheelUnit::Page => delta * 3.0,
                    };
                    let limit = |n: f32| n.clamp(-10.0, 10.0);
                    sent.push(Input::Scroll {
                        x: limit(notches.x),
                        y: limit(notches.y),
                    });
                }
                Event::Key {
                    key,
                    physical_key,
                    pressed,
                    ..
                } => {
                    // The key's place on the keyboard: the sharer's layout picks the letter.
                    let name = shortcut_key(physical_key.unwrap_or(key).name());
                    if scan_code(name).is_none() {
                        continue;
                    }
                    if pressed {
                        self.keys.insert(name.to_owned());
                    } else if !self.keys.remove(name) {
                        continue;
                    }
                    sent.push(Input::Key {
                        key: name.to_owned(),
                        down: pressed,
                    });
                }
                // The window turns Ctrl+C, X and V into these: press and release the key.
                Event::Copy | Event::Cut | Event::Paste(_) => {
                    let key = match event {
                        Event::Copy => "C",
                        Event::Cut => "X",
                        _ => "V",
                    };
                    for down in [true, false] {
                        sent.push(Input::Key {
                            key: key.into(),
                            down,
                        });
                    }
                }
                _ => {}
            }
        }
        if let Some((x, y)) = moved {
            sent.insert(0, Input::Move { x, y });
        }
        let now = ui.input(|input| input.time);
        if ui.input(|input| input.key_down(Key::Escape)) {
            let since = *self.escape_since.get_or_insert(now);
            if now - since >= ESCAPE_HOLD {
                self.escape_since = None;
                sent.extend(self.release());
                return (sent, true);
            }
            ui.ctx().request_repaint();
        } else {
            self.escape_since = None;
        }
        (sent, false)
    }
}

/// A Mac's Command key goes as Ctrl, so Command+C copies on a Windows or Linux
/// sharer; a Mac sharer turns Ctrl back into Command. The Mac's own Control key also
/// goes as Ctrl, for Ctrl+C in a terminal elsewhere.
fn shortcut_key(name: &str) -> &str {
    match name {
        "SuperLeft" if cfg!(target_os = "macos") => "ControlLeft",
        "SuperRight" if cfg!(target_os = "macos") => "ControlRight",
        name => name,
    }
}

/// Once per frame: answer for the sharer when the prompt timed out, give up asking
/// after no answer, and stop controlling a screen no longer watched.
pub fn frame(home: &mut Home, now: f64) {
    let timeout = REQUEST_TIMEOUT.as_secs_f64();
    let control = &mut home.voice.control;
    let mut commands = Vec::new();
    if let Some((_, request)) = control.view.asked
        && now - control.asked_at > timeout
    {
        control.view.asked = None;
        commands.push(Command::AnswerControl(request, false));
    }
    if let Some((sid, on)) = control.view.screen.clone() {
        let watched = home.voice.watching.as_ref().is_some_and(|w| w.sid == sid);
        // A little longer than the sharer waits, so their answer can still arrive.
        let unanswered = !on && now - control.asking_at > timeout + 2.0;
        if !watched || unanswered {
            control.view.screen = None;
            commands.push(Command::ReleaseControl);
        }
        if unanswered {
            let name = sharer_name(home, &sid).unwrap_or_else(|| t!("remote.they").into());
            home.toast = Some((t!("remote.no_answer", name = name), now + 6.0));
        }
    }
    if let Some(server) = call_server(home) {
        for command in commands {
            server.send(command);
        }
    }
}

/// Whose screen this is (track ID), in the call.
fn sharer_name(home: &Home, sid: &str) -> Option<String> {
    let server = call_server(home)?;
    let call = home.voice.call.as_ref()?;
    let peers = server.voice.get(&call.channel)?;
    let peer = peers
        .iter()
        .find(|peer| peer.screens.iter().any(|s| s.sid == sid))?;
    Some(peer.name(server))
}

/// The note when asking for, or controlling, a screen ended.
pub fn ended(home: &Home, sid: &str, why: Option<Refusal>) -> String {
    let name = sharer_name(home, sid).unwrap_or_else(|| t!("remote.the_sharer").into());
    match why {
        Some(Refusal::Denied) => t!("remote.denied", name = name),
        Some(Refusal::Busy) => t!("remote.busy", name = name),
        Some(Refusal::Unsupported) => t!("remote.unsupported", name = name),
        None => t!("remote.taken_back", name = name),
    }
}

fn person_name(server: &Server, person: &crate::server::Id) -> String {
    server.member_name(person)
}

/// The sharer's side: "Ada wants to control your screen", and while someone has
/// control, a bar across the window to take it back. Draw before the panels.
pub fn sharer(ui: &mut Ui, home: &mut Home) {
    let Some(server) = call_server(home) else {
        return;
    };
    let view = &home.voice.control.view;
    let by = view.by.map(|person| person_name(server, &person));
    let asked = view
        .asked
        .map(|(person, request)| (person_name(server, &person), request));
    let mut command = None;
    if let Some(name) = by {
        let c = ui::colors(ui);
        egui::Panel::top("remote control")
            .frame(
                Frame::new()
                    .fill(c.danger)
                    .inner_margin(Margin::symmetric(ui::SPACE_M as i8, 6)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    let text = format!("🖱 {}", t!("remote.controlling_you", name = name));
                    ui.label(ui::bold(text).color(ui::ON_FILL));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui::secondary(ui, t!("remote.stop")).clicked() {
                            command = Some(Command::StopControl);
                        }
                    });
                });
            });
    }
    if let Some((name, request)) = asked {
        let now = ui.input(|input| input.time);
        let ready = now - home.voice.control.asked_at >= ALLOW_DELAY;
        if !ready {
            ui.ctx().request_repaint();
        }
        let title = t!("remote.request_title", name = name);
        let (answer, closed) = ui::dialog(ui.ctx(), "remote control", &title, |ui| {
            ui.add(egui::Label::new(t!("remote.allow_help")).wrap());
            let mut answer = None;
            ui::footer(ui, |ui| {
                if ui
                    .add_enabled_ui(ready, |ui| ui::primary(ui, t!("remote.allow")))
                    .inner
                    .clicked()
                {
                    answer = Some(true);
                }
                if ui::secondary(ui, t!("remote.deny")).clicked() {
                    answer = Some(false);
                }
            });
            answer
        });
        // Closing the prompt any other way is a no.
        if let Some(allow) = answer.or(closed.then_some(false)) {
            home.voice.control.view.asked = None;
            command = Some(Command::AnswerControl(request, allow));
        }
    }
    if let (Some(command), Some(server)) = (command, call_server(home)) {
        server.send(command);
    }
}
