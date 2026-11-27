//! NSFW channels (#681): a channel manager marks a channel as possibly holding mature
//! content. The first time someone opens it on a device, a warning takes the chat pane
//! until they continue or go back. The answer stays on this device only
//! (`nsfw_agreed.json`): it is never synced or sent to a server.
//!
//! For sensitive media (#682): `Server::is_nsfw` says whether a channel is marked, and
//! `Home::agreed` whether this device agreed to see it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use eframe::egui::{self, Align2, Rect, Role, Ui, UiBuilder, Vec2, WidgetInfo};

use super::{Home, focus};
use crate::{server::Id, ui};

/// The channels agreed to on this device, by server address and channel.
#[derive(Default)]
pub struct Agreed {
    set: BTreeSet<(String, Id)>,
    path: Option<PathBuf>,
}

impl Agreed {
    pub fn open(path: PathBuf) -> Self {
        let set = crate::load_json(&path).unwrap_or_else(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "NSFW answers unreadable");
            BTreeSet::new()
        });
        Self {
            set,
            path: Some(path),
        }
    }

    pub fn agreed(&self, server: &url::Url, channel: &Id) -> bool {
        self.set.contains(&(server.to_string(), *channel))
    }

    pub fn agree(&mut self, server: &url::Url, channel: Id) {
        if !self.set.insert((server.to_string(), channel)) {
            return;
        }
        let Some(path) = &self.path else {
            return;
        };
        let dir = path.parent().expect("in the data folder");
        if let Err(error) = rusttalk_core::client::save_json(dir, path, &self.set) {
            tracing::warn!(error = %format_args!("{error:#}"), "saving NSFW answers failed");
        }
    }
}

impl Home {
    /// Whether this device agreed to see the NSFW channel `channel` on server `index`.
    pub fn agreed(&self, index: usize, channel: &Id) -> bool {
        self.nsfw_agreed
            .agreed(&self.servers[index].address, channel)
    }

    /// Whether `channel` waits behind the warning on this device.
    pub(super) fn nsfw_warns(&self, index: usize, channel: &Id) -> bool {
        self.servers[index].is_nsfw(channel) && !self.agreed(index, channel)
    }
}

/// The NSFW switch in channel settings. Returns whether it changed.
pub fn toggle(ui: &mut Ui, on: &mut bool) -> bool {
    ui::toggle_row(ui, on, t!("nsfw.title"), t!("nsfw.help")).changed()
}

/// The "NSFW" badge in a channel's header.
pub fn header_badge(ui: &mut Ui, nsfw: bool) {
    if nsfw {
        badge(ui);
    }
}

fn badge(ui: &mut Ui) {
    let tip = t!("nsfw.badge_help");
    ui::badge(ui, t!("nsfw.badge"), ui::colors(ui).danger)
        .on_hover_text(tip)
        .widget_info(|| WidgetInfo::labeled(Role::Image, true, tip));
}

/// The room the badge takes after a channel's name in the list.
pub fn badge_room(ui: &Ui) -> f32 {
    focus::row_badge_room(ui, t!("nsfw.badge"))
}

/// The badge right after `name` in a channel `row` of the list (see `focus::badge`).
pub fn row_badge(ui: &mut Ui, row: Rect, name: &str, selected: bool, trailing: f32) {
    let rect = focus::row_badge_rect(ui, row, name, selected, trailing, t!("nsfw.badge"));
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    badge(&mut child);
}

/// The warning in place of an NSFW channel's chat, until this device continues or goes back.
pub(super) fn warning(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    super::channel_title(ui, home, index, channel);
    let mut agree = false;
    let mut back = false;
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.2);
        ui::max_width(ui, 420.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(48.0), egui::Sense::hover());
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "🔞",
            egui::FontId::proportional(40.0),
            ui::colors(ui).strong,
        );
        ui.add_space(ui::SPACE_S);
        ui.label(ui::heading(t!("nsfw.warning_title")));
        ui.add_space(ui::SPACE_S);
        ui.label(t!("nsfw.warning"));
        ui.label(ui::caption(t!("nsfw.warning_remembered")));
        ui.add_space(ui::SPACE_M);
        agree = ui::primary(ui, t!("nsfw.continue")).clicked();
        ui.add_space(ui::SPACE_S);
        back = ui::secondary(ui, t!("nsfw.go_back")).clicked();
    });
    if agree {
        let address = home.servers[index].address.clone();
        home.nsfw_agreed.agree(&address, channel);
    } else if back {
        home.servers[index].open = None;
        if home.layout.phone {
            home.layout.list = true;
        }
    }
}
