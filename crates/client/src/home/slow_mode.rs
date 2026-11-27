//! Slow mode: a channel setting that makes each member wait between messages.

use std::time::Duration;

use egui::{ComboBox, Role, Ui, WidgetInfo};

use crate::{
    server::{Chat, Directory},
    ui,
};

/// The choices, in seconds, as the server takes them; 0 is off.
pub const CHOICES: [u32; 9] = [0, 5, 10, 30, 60, 300, 900, 3600, 21600];

/// "Off", "30 s", "5 min" or "6 h".
pub fn label(secs: u32) -> String {
    match secs {
        0 => t!("slow_mode.off").into(),
        s if s % 3600 == 0 => t!("slow_mode.hours", hours = s / 3600),
        s if s % 60 == 0 => t!("slow_mode.minutes", minutes = s / 60),
        s => t!("slow_mode.seconds", seconds = s),
    }
}

/// The seconds this person waits between messages in `channel`: 0 when slow mode is off
/// there or they may bypass it.
pub fn wait(dir: &Directory, channel: &crate::server::Id) -> u32 {
    if dir.perms.bypass_slow_mode {
        return 0;
    }
    (dir.categories.iter())
        .flat_map(|category| &category.channels)
        .find(|known| known.id == *channel)
        .map_or(0, |known| known.slow_mode_secs)
}

/// Whole seconds until this person may send in `chat` again, if they must still wait.
pub fn left(chat: &Chat, now: f64) -> Option<u64> {
    let left = (chat.slow_until? - now).ceil();
    (left > 0.0).then_some(left as u64)
}

/// The line above the message box while waiting, which counts down.
pub fn countdown(ui: &mut Ui, seconds: u64) {
    ui.weak(t!("slow_mode.wait", seconds = seconds));
    ui.ctx().request_repaint_after(Duration::from_millis(250));
}

/// The small icon in a channel's header while slow mode is on there.
pub fn header_icon(ui: &mut Ui, secs: u32) {
    if secs == 0 {
        return;
    }
    let tip = t!("slow_mode.header_tip", time = label(secs));
    ui.weak("⏱")
        .on_hover_text(&tip)
        .widget_info(|| WidgetInfo::labeled(Role::Image, true, &tip));
}

/// The slow mode choice in channel settings. Returns whether it changed.
pub fn picker(ui: &mut Ui, secs: &mut u32) -> bool {
    let title = t!("slow_mode.title");
    let before = *secs;
    ui::setting_row(ui, title, t!("slow_mode.help"), |ui| {
        let combo = ComboBox::from_id_salt("slow mode")
            .selected_text(label(*secs))
            .show_ui(ui, |ui| {
                for choice in CHOICES {
                    ui.selectable_value(secs, choice, label(choice));
                }
            });
        combo
            .response
            .widget_info(|| WidgetInfo::labeled(Role::ComboBox, true, title));
    });
    *secs != before
}
