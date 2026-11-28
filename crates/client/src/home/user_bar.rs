//! The bar at the bottom left: your picture, name and status (opening the status
//! picker), mic and sound buttons while in a call, and Settings.

use eframe::egui::{
    Align, Color32, FontSelection, Label, Layout, TextWrapMode, Ui, Vec2, WidgetText,
};
use rusttalk_core::presence::{Presence, Status as Choice};

use super::{Directory, Home, Tab, View, face_speaking, presence_color};
use crate::{
    call::call_server,
    net::Command,
    ui::{self, ICON_BUTTON, Item, Look, Palette},
};

const AVATAR_SIZE: f32 = 32.0;
/// The message bar's height, plus the difference between the chat's margin
/// (`SPACE_M`) and this panel's (`SPACE_S`) on both sides: both centres line up.
const BAR_HEIGHT: f32 = super::COMPOSER_HEIGHT + 2.0 * (ui::SPACE_M - ui::SPACE_S);
const BUTTON_SIZE: Vec2 = Vec2::new(30.0, ICON_BUTTON);

/// Label and one-line hint of a status.
fn status_text(choice: Choice) -> (&'static str, &'static str) {
    match choice {
        Choice::Online => (t!("status.online"), t!("status.online_help")),
        Choice::Away => (t!("status.away"), t!("status.away_help")),
        Choice::Dnd => (t!("status.dnd"), t!("status.dnd_help")),
        Choice::Invisible => (t!("status.invisible"), t!("status.invisible_help")),
    }
}

fn status_color(c: &Palette, choice: Choice) -> Color32 {
    presence_color(
        c,
        match choice {
            Choice::Online => Presence::Online,
            Choice::Away => Presence::Away,
            Choice::Dnd => Presence::Dnd,
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
        let gear = settings_button(ui, update_available);
        if gear.clicked() {
            home.settings = Some(if update_available {
                Tab::Updates
            } else {
                Tab::Profile
            });
        }
        if let Some(Some(tab)) =
            ui::button_menu(&gear, false, |m| settings_menu(m, update_available))
        {
            home.settings = Some(tab);
        }
        if home.voice.call.is_some() {
            sound_buttons(ui, home);
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let me = identity(ui, home, home.shown_status());
            ui::menu_wide(&me, |m| picked = status_picker(m, status));
        });
    });
    if let Some(choice) = picked {
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
    let speaking = home.voice.local_speaking(&home.servers);
    ui::row_button(
        ui,
        BAR_HEIGHT,
        &t!("status.button", name = name, status = label),
        |ui| {
            ui.spacing_mut().item_spacing.x = ui::SPACE_S;
            let avatar = face_speaking(ui, dir, me, &name, AVATAR_SIZE, false, speaking);
            ui::presence_dot(ui, avatar.rect, 5.0, color, c.side, status == Choice::Dnd);
            let width = ui.available_width().max(0.0);
            let [name, status] = [ui::bold(&name), ui::caption(label)].map(|text| {
                WidgetText::from(text).into_galley(
                    ui,
                    Some(TextWrapMode::Truncate),
                    width,
                    FontSelection::Default,
                )
            });
            let size = Vec2::new(width, name.size().y + status.size().y);
            ui.allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.add(Label::new(name));
                ui.add(Label::new(status));
            });
        },
    )
}

/// Rows with each status, its hint and a check on the current one.
fn status_picker(m: &mut ui::Menu, current: Choice) -> Option<Choice> {
    let mut picked = None;
    m.caption(t!("status.set"));
    let c = ui::colors(m.ui());
    for choice in [Choice::Online, Choice::Away, Choice::Dnd, Choice::Invisible] {
        let (label, hint) = status_text(choice);
        let mut item = Item::dot(status_color(c, choice), label)
            .hint(hint)
            .picked(choice == current);
        if choice == Choice::Dnd {
            item = item.barred();
        }
        if m.add(item).clicked() {
            picked = Some(choice);
        }
    }
    picked
}

/// A right-click on the gear: the pages of Settings, grouped as in its sidebar.
fn settings_menu(m: &mut ui::Menu, update_available: bool) -> Option<Tab> {
    let mut picked = None;
    for (at, group) in crate::settings::groups(update_available).iter().enumerate() {
        if at > 0 {
            m.separator();
        }
        m.caption(group.title);
        for page in &group.pages {
            if m.item(page.icon, page.label).clicked() {
                picked = Some(page.id);
            }
        }
    }
    picked
}

/// Mic and sound on or off; only shown in a call.
/// A right-click on either picks its device.
fn sound_buttons(ui: &mut Ui, home: &mut Home) {
    let sound = deafen_button(ui, home, BUTTON_SIZE);
    crate::call::device_choices(&sound, home, false);
    let mic = mic_button(ui, home, BUTTON_SIZE);
    crate::call::device_choices(&mic, home, true);
}

fn struck(off: bool) -> Look {
    if off { Look::Struck } else { Look::Plain }
}

/// Sound on or off, also in the call view's controls on phones.
pub fn deafen_button(ui: &mut Ui, home: &Home, size: Vec2) -> eframe::egui::Response {
    let deafened = home.voice.audio.deafened();
    let button = ui::icon_button(ui, "🎧", deafen_tip(deafened), struck(deafened), size);
    if button.clicked() {
        toggle_deafen(home);
    }
    button
}

/// Mic on or off, also in the call view's controls on phones.
pub fn mic_button(ui: &mut Ui, home: &Home, size: Vec2) -> eframe::egui::Response {
    let audio = &home.voice.audio;
    let muted = audio.muted() || audio.deafened();
    let server_muted = server_muted(home);
    let mic = ui
        .add_enabled_ui(!server_muted, |ui| {
            let off = muted || server_muted;
            ui::icon_button(ui, "🎤", mute_tip(off), struck(off), size)
        })
        .inner
        .on_disabled_hover_text(t!("call.server_muted"));
    if mic.clicked() {
        toggle_mute(home);
    }
    mic
}

/// A moderator's mute holds until they lift it; the person can't unmute themselves.
pub fn server_muted(home: &Home) -> bool {
    call_server(home).is_some_and(|server| server.voice_muted(&server.dir.me))
}

/// The mic button, also for the global shortcut. Unmuting also undeafens.
pub fn toggle_mute(home: &Home) {
    let audio = &home.voice.audio;
    set_mute(home, !(audio.muted() || audio.deafened()));
}

/// Mute or unmute, also from the phone's call controls (`call::callkit`).
pub fn set_mute(home: &Home, on: bool) {
    if server_muted(home) {
        return;
    }
    set_mic(&home.voice.audio, on);
    sync_voice_status(home);
}

/// Mic off (`on`) or on; turning it on also undeafens.
fn set_mic(audio: &crate::audio::Audio, on: bool) {
    audio.set_muted(on);
    if !on {
        audio.set_deafened(false);
    }
}

/// Mic on or off; deafened counts as off, so turning it on also undeafens. Also for
/// the Android call notification's Mute button (`call::notice`).
#[cfg(any(target_os = "android", test))]
pub fn toggle_mic(audio: &crate::audio::Audio) {
    set_mic(audio, !(audio.muted() || audio.deafened()));
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
