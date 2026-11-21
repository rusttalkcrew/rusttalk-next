//! The bar at the bottom left: your picture, name and status (opening the status
//! picker), mic and sound buttons while in a call, and Settings.

use eframe::egui::{Align, Color32, Label, Layout, RichText, Ui, Vec2};
use rusttalk_core::presence::Status as Choice;

use super::{AWAY, Directory, Home, OFFLINE, ONLINE, Tab, View, face, palette};
use crate::widgets::{self, AMBER, BUTTON_HEIGHT, GAP, Look};

const AVATAR_SIZE: f32 = 32.0;
const BAR_HEIGHT: f32 = 40.0;
const STATUS_ROW_HEIGHT: f32 = 44.0;
const BUTTON_SIZE: Vec2 = Vec2::new(30.0, BUTTON_HEIGHT);
const UPDATE_BADGE: Color32 = AMBER;

/// Colour, label and one-line hint of a status.
fn status_look(choice: Choice) -> (Color32, &'static str, &'static str) {
    match choice {
        Choice::Online => (ONLINE, "Online", "People see that you're here"),
        Choice::Away => (AWAY, "Away", "People see that you stepped away"),
        Choice::Invisible => (
            OFFLINE,
            "Invisible",
            "You appear offline but can still use RustTalk",
        ),
    }
}

fn mute_tip(muted: bool) -> &'static str {
    if muted { "Unmute" } else { "Mute" }
}

fn deafen_tip(deafened: bool) -> &'static str {
    if deafened { "Undeafen" } else { "Deafen" }
}

pub(super) fn show(ui: &mut Ui, home: &mut Home, update_available: bool) {
    let status = home.status.unwrap_or(Choice::Online);
    let mut picked = None;
    widgets::right_row(ui, BAR_HEIGHT, |ui| {
        ui.spacing_mut().item_spacing.x = GAP;
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
            widgets::popup(&me, |ui| picked = status_picker(ui, status));
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
        View::Server(index) | View::Direct(Some((index, _))) => home.servers.get(index),
        _ => None,
    };
    // The name is per server; without an open one, use any server's.
    let server = open.or_else(|| home.servers.iter().find(|s| s.member(me).is_some()));
    let name = server.map_or_else(|| "You".to_owned(), |s| s.member_name(me));
    // The picture is the same everywhere; show it from any server that has it.
    let none = Directory::default();
    let dir = (home.servers.iter())
        .find(|s| s.member(me).is_some_and(|m| m.avatar.is_some()))
        .map_or(&none, |s| &s.dir);
    let (color, label, _) = status_look(status);
    widgets::row_button(
        ui,
        BAR_HEIGHT,
        &format!("{name}, {label}. Change status"),
        |ui| {
            let avatar = face(ui, dir, me, &name, AVATAR_SIZE, false);
            let radius = 5.0;
            let center = avatar.rect.right_bottom() - Vec2::splat(radius * 0.8);
            ui.painter()
                .circle(center, radius, color, (2.0, palette(ui).side));
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add(Label::new(RichText::new(&name).strong()).truncate());
                ui.add(Label::new(RichText::new(label).small().weak()).truncate());
            });
        },
    )
}

/// Rows with each status, its hint and a check on the current one.
fn status_picker(ui: &mut Ui, current: Choice) -> Option<Choice> {
    let mut picked = None;
    ui.label(RichText::new("Set your status").strong());
    for choice in [Choice::Online, Choice::Away, Choice::Invisible] {
        let (color, label, hint) = status_look(choice);
        let row = widgets::row_button(ui, STATUS_ROW_HEIGHT, label, |ui| {
            widgets::right_row(ui, STATUS_ROW_HEIGHT, |ui| {
                // Always take the space, so rows line up.
                let check = if choice == current { "✔" } else { " " };
                ui.label(RichText::new(check).color(color));
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    widgets::dot(ui, color);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(label);
                        ui.add(Label::new(RichText::new(hint).small().weak()).truncate());
                    });
                });
            });
        });
        if row.clicked() {
            picked = Some(choice);
            ui.close();
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
    let ears = widgets::icon_button(ui, "🎧", deafen_tip(deafened), look(deafened), BUTTON_SIZE);
    if ears.clicked() {
        audio.set_deafened(!deafened);
    }
    let mic = widgets::icon_button(ui, "🎤", mute_tip(muted), look(muted), BUTTON_SIZE);
    if mic.clicked() {
        audio.set_muted(!muted);
        if muted {
            audio.set_deafened(false);
        }
    }
}

/// The gear, with a dot while an update waits.
fn settings_button(ui: &mut Ui, update_available: bool) -> eframe::egui::Response {
    let tip = if update_available {
        "Settings · Update available"
    } else {
        "Settings"
    };
    let button = widgets::icon_button(ui, "⚙", tip, Look::Plain, BUTTON_SIZE);
    if update_available {
        let center = button.rect.right_top() + Vec2::new(-6.0, 6.0);
        ui.painter().circle_filled(center, 4.0, UPDATE_BADGE);
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
        let (_, label, hint) = status_look(Choice::Invisible);
        assert_eq!(label, "Invisible");
        assert!(hint.contains("appear offline"));
    }
}
