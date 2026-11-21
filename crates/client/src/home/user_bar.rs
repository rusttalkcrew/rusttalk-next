//! The bar at the bottom left: your picture, name and status (opening the status
//! picker), mic and sound buttons while in a call, and Settings.

use eframe::egui::{Align, Color32, Label, Layout, Ui, Vec2};
use rusttalk_core::presence::{Presence, Status as Choice};

use super::{Directory, Home, Tab, View, face, presence_color};
use crate::{
    call::call_server,
    net::Command,
    ui::{self, ICON_BUTTON, Item, Look, Palette},
};

const AVATAR_SIZE: f32 = 32.0;
const BAR_HEIGHT: f32 = 40.0;
const BUTTON_SIZE: Vec2 = Vec2::new(30.0, ICON_BUTTON);

/// Label and one-line hint of a status.
fn status_text(choice: Choice) -> (&'static str, &'static str) {
    match choice {
        Choice::Online => (t!("status.online"), t!("status.online_help")),
        Choice::Away => (t!("status.away"), t!("status.away_help")),
        Choice::Invisible => (t!("status.invisible"), t!("status.invisible_help")),
    }
}

fn status_color(c: &Palette, choice: Choice) -> Color32 {
    presence_color(
        c,
        match choice {
            Choice::Online => Presence::Online,
            Choice::Away => Presence::Away,
            Choice::Invisible => Presence::Offline,
        },
    )
}

fn mute_tip(muted: bool) -> &'static str {
    if muted {
        t!("call.unmute")
    } else {
        t!("call.mute")
    }
}

fn deafen_tip(deafened: bool) -> &'static str {
    if deafened {
        t!("call.undeafen")
    } else {
        t!("call.deafen")
    }
}

pub(super) fn show(ui: &mut Ui, home: &mut Home, update_available: bool) {
    let status = home.status.unwrap_or(Choice::Online);
    let mut picked = None;
    ui::right_row(ui, BAR_HEIGHT, |ui| {
        ui.spacing_mut().item_spacing.x = ui::SPACE_XS;
        if settings_button(ui, update_available).clicked() {
            home.settings = Some(if update_available {
                Tab::Updates
            } else {
                Tab::Profile
            });
        }
        if home.voice.call.is_some() {
            sound_buttons(ui, home);
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let me = identity(ui, home, status);
            ui::menu_wide(&me, |m| picked = status_picker(m, status));
        });
    });
    if let Some(choice) = picked {
        home.auto_away = false;
        home.set_status(choice);
    }
}

/// Picture with the status dot, name on the open server and the status under it.
fn identity(ui: &mut Ui, home: &Home, status: Choice) -> eframe::egui::Response {
    let me = home.identity.certificate.person_id();
    let open = match home.view {
        View::Server(index) => home.servers.get(index),
        View::Direct(Some(peer)) => home.dm_server(&peer).and_then(|i| home.servers.get(i)),
        _ => None,
    };
    // The name is per server; without an open one, use any server's.
    let server = open.or_else(|| home.servers.iter().find(|s| s.member(me).is_some()));
    let name = server.map_or_else(|| t!("common.you").to_owned(), |s| s.member_name(me));
    // The picture is the same everywhere; show it from any server that has it.
    let none = Directory::default();
    let dir = (home.servers.iter())
        .find(|s| s.member(me).is_some_and(|m| m.avatar.is_some()))
        .map_or(&none, |s| &s.dir);
    let c = ui::colors(ui);
    let color = status_color(c, status);
    let (label, _) = status_text(status);
    ui::row_button(
        ui,
        BAR_HEIGHT,
        &t!("status.button", name = name, status = label),
        |ui| {
            let avatar = face(ui, dir, me, &name, AVATAR_SIZE, false);
            ui::presence_dot(ui, avatar.rect, 5.0, color, c.side);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add(Label::new(ui::bold(&name)).truncate());
                ui.add(Label::new(ui::caption(label)).truncate());
            });
        },
    )
}

/// Rows with each status, its hint and a check on the current one.
fn status_picker(m: &mut ui::Menu, current: Choice) -> Option<Choice> {
    let mut picked = None;
    m.caption(t!("status.set"));
    let c = ui::colors(m.ui());
    for choice in [Choice::Online, Choice::Away, Choice::Invisible] {
        let (label, hint) = status_text(choice);
        let item = Item::dot(status_color(c, choice), label)
            .hint(hint)
            .picked(choice == current);
        if m.add(item).clicked() {
            picked = Some(choice);
        }
    }
    picked
}

/// Mic and sound on or off; only shown in a call.
fn sound_buttons(ui: &mut Ui, home: &Home) {
    let audio = &home.voice.audio;
    let deafened = audio.deafened();
    let muted = audio.muted() || deafened;
    let look = |off: bool| if off { Look::Struck } else { Look::Plain };
    if ui::icon_button(ui, "🎧", deafen_tip(deafened), look(deafened), BUTTON_SIZE).clicked() {
        toggle_deafen(home);
    }
    let server_muted = server_muted(home);
    let mic = ui
        .add_enabled_ui(!server_muted, |ui| {
            let off = muted || server_muted;
            ui::icon_button(ui, "🎤", mute_tip(off), look(off), BUTTON_SIZE)
        })
        .inner
        .on_disabled_hover_text(t!("call.server_muted"));
    if mic.clicked() {
        toggle_mute(home);
    }
}

/// A moderator's mute holds until they lift it; the person can't unmute themselves.
pub fn server_muted(home: &Home) -> bool {
    call_server(home).is_some_and(|server| server.voice_muted(&server.dir.me))
}

/// The mic button, also for the global shortcut. Unmuting also undeafens.
pub fn toggle_mute(home: &Home) {
    if server_muted(home) {
        return;
    }
    let audio = &home.voice.audio;
    let muted = audio.muted() || audio.deafened();
    audio.set_muted(!muted);
    if muted {
        audio.set_deafened(false);
    }
    sync_voice_status(home);
}

pub fn toggle_deafen(home: &Home) {
    let audio = &home.voice.audio;
    audio.set_deafened(!audio.deafened());
    sync_voice_status(home);
}

fn sync_voice_status(home: &Home) {
    if let Some(server) = call_server(home) {
        server.send(Command::SyncVoiceStatus);
    }
}

/// The gear, with a dot while an update waits.
fn settings_button(ui: &mut Ui, update_available: bool) -> eframe::egui::Response {
    let tip = if update_available {
        t!("settings.title_update")
    } else {
        t!("settings.title")
    };
    let button = ui::icon_button(ui, "⚙", tip, Look::Plain, BUTTON_SIZE);
    if update_available {
        let center = button.rect.right_top() + Vec2::new(-6.0, 6.0);
        ui.painter()
            .circle_filled(center, 4.0, ui::colors(ui).warning);
    }
    button
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sound_tips_follow_the_state() {
        assert_eq!(mute_tip(false), "Mute");
        assert_eq!(mute_tip(true), "Unmute");
        assert_eq!(deafen_tip(false), "Deafen");
        assert_eq!(deafen_tip(true), "Undeafen");
    }

    #[test]
    fn invisible_explains_itself() {
        let (label, hint) = status_text(Choice::Invisible);
        assert_eq!(label, "Invisible");
        assert!(hint.contains("appear offline"));
    }
}
