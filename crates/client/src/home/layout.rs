//! Sidebar widths and the member list toggle, remembered on this device only.

use std::ops::RangeInclusive;

use egui::{Id, Rect, Sense, Stroke, Ui, Vec2, WidgetInfo, accesskit::Role};

use crate::{
    audio::{Audio, Settings},
    ui::{self, Look},
};

pub const CHANNELS_WIDTH: f32 = 240.0;
pub const CHANNELS_RANGE: RangeInclusive<f32> = 180.0..=360.0;
pub const MEMBERS_WIDTH: f32 = 220.0;
pub const MEMBERS_RANGE: RangeInclusive<f32> = 180.0..=320.0;
/// Narrower windows hide the member list unless it is opened by hand.
const NARROW_WINDOW: f32 = 900.0;
/// How far from the edge the resize handle reaches on either side.
const GRAB: f32 = 3.0;

#[derive(Clone, Copy, PartialEq)]
struct Saved {
    channels: f32,
    members: f32,
    members_open: bool,
}

pub struct Layout {
    pub channels: f32,
    pub members: f32,
    /// The person's choice for windows wide enough for the member list.
    members_open: bool,
    /// Opened by hand in a narrow window; forgotten when the window size class changes.
    peek: bool,
    narrow: bool,
    /// What the settings file holds, so it is written only after a change.
    saved: Saved,
}

impl Layout {
    pub fn new(settings: &Settings) -> Self {
        let saved = Saved {
            channels: width(settings.channels_width, CHANNELS_WIDTH, CHANNELS_RANGE),
            members: width(settings.members_width, MEMBERS_WIDTH, MEMBERS_RANGE),
            members_open: !settings.members_hidden,
        };
        Self {
            channels: saved.channels,
            members: saved.members,
            members_open: saved.members_open,
            peek: false,
            narrow: false,
            saved,
        }
    }

    pub fn set_window_width(&mut self, window: f32) {
        let narrow = window < NARROW_WINDOW;
        if narrow != self.narrow {
            self.peek = false;
        }
        self.narrow = narrow;
    }

    pub fn members_shown(&self) -> bool {
        if self.narrow {
            self.peek
        } else {
            self.members_open
        }
    }

    fn toggle_members(&mut self) {
        if self.narrow {
            self.peek = !self.peek;
        } else {
            self.members_open = !self.members_open;
        }
    }

    /// Write a changed layout to this device's settings; never synced to other devices.
    pub fn save(&mut self, audio: &Audio) {
        let now = Saved {
            channels: self.channels,
            members: self.members,
            members_open: self.members_open,
        };
        if now == self.saved {
            return;
        }
        self.saved = now;
        let mut settings = audio.settings();
        settings.channels_width = now.channels;
        settings.members_width = now.members;
        settings.members_hidden = !now.members_open;
        audio.set_settings(settings);
    }
}

/// A stored width, or the default when it is missing or broken.
fn width(stored: f32, default: f32, range: RangeInclusive<f32>) -> f32 {
    if stored.is_finite() && stored > 0.0 {
        stored.clamp(*range.start(), *range.end())
    } else {
        default
    }
}

/// The people button in a channel header that shows or hides the member list.
pub fn members_button(ui: &mut Ui, layout: &mut Layout) {
    let shown = layout.members_shown();
    let tip = if shown {
        "Hide members"
    } else {
        "Show members"
    };
    // A soft fill while the list is open, like a pressed toggle.
    let look = if shown { Look::Plain } else { Look::Ghost };
    if ui::icon_button(ui, "👥", tip, look, Vec2::new(30.0, 26.0)).clicked() {
        layout.toggle_members();
    }
}

/// A drag handle on a sidebar's inner edge. Double-click resets the default width.
/// `panel` is the sidebar as shown this frame; returns whether it is being dragged.
pub fn resize_edge(
    ui: &Ui,
    label: &'static str,
    panel: Rect,
    left: bool,
    width: &mut f32,
    default: f32,
    range: RangeInclusive<f32>,
) -> bool {
    let x = if left { panel.right() } else { panel.left() };
    let rect = Rect::from_x_y_ranges(x - GRAB..=x + GRAB, panel.y_range());
    // Registered after the panels next to it, so it wins the pointer over their contents.
    let response = ui.interact(rect, Id::unique(label), Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::labeled(Role::Splitter, true, label));
    if response.double_clicked() {
        *width = default;
    } else if response.dragged()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let wanted = if left {
            pointer.x - panel.left()
        } else {
            panel.right() - pointer.x
        };
        *width = wanted.clamp(*range.start(), *range.end());
    }
    if response.hovered() || response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        let line = Stroke::new(2.0, ui::accent(ui).base);
        ui.painter().vline(x, panel.y_range(), line);
    }
    response.dragged()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_widths_fall_back_and_are_clamped() {
        let settings = Settings {
            channels_width: f32::NAN,
            members_width: 9_000.0,
            ..Settings::default()
        };
        let layout = Layout::new(&settings);
        assert_eq!(layout.channels, CHANNELS_WIDTH);
        assert_eq!(layout.members, *MEMBERS_RANGE.end());
    }

    #[test]
    fn narrow_windows_hide_members_without_forgetting_the_choice() {
        let mut layout = Layout::new(&Settings::default());
        layout.set_window_width(1200.0);
        assert!(layout.members_shown());
        layout.set_window_width(800.0);
        assert!(!layout.members_shown());
        // Opened by hand while narrow: shown, but not the remembered choice.
        layout.toggle_members();
        assert!(layout.members_shown());
        assert!(layout.members_open);
        layout.set_window_width(1200.0);
        layout.set_window_width(800.0);
        assert!(!layout.members_shown());
        layout.set_window_width(1200.0);
        layout.toggle_members();
        assert!(!layout.members_shown());
        assert!(!layout.members_open);
    }
}
