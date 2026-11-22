//! Sidebar widths, the member list toggle and the voice channel split, remembered on
//! this device only.

use std::ops::RangeInclusive;

use egui::{Id, Rect, Sense, Stroke, Ui, Vec2, WidgetInfo, accesskit::Role, pos2};

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
/// The call's share of a voice channel's space, and how far the divider can go.
pub const SPLIT: f32 = 0.55;
pub const SPLIT_RANGE: RangeInclusive<f32> = 0.2..=0.8;
/// The gap between call and chat that holds the divider.
const DIVIDER: f32 = 12.0;
const SPLIT_BUTTON: Vec2 = Vec2::new(30.0, 26.0);

#[derive(Clone, Copy, PartialEq)]
struct Saved {
    channels: f32,
    members: f32,
    members_open: bool,
    split: Split,
}

/// How a voice channel shares its space between the call and its chat.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Split {
    /// Chat on the right instead of below.
    pub side_by_side: bool,
    /// The call's share, within `SPLIT_RANGE`.
    pub ratio: f32,
    pub chat_hidden: bool,
    pub stage_hidden: bool,
    /// Only cameras and screens on the stage.
    pub video_only: bool,
    /// The overlays on screen tiles (see `call::overlay`).
    pub show_fps: bool,
    pub nerd_stats: bool,
}

impl Split {
    fn new(settings: &Settings) -> Self {
        Self {
            side_by_side: settings.call_side_by_side,
            ratio: ratio(settings.call_split),
            chat_hidden: settings.call_chat_hidden,
            // Never both hidden: the call wins.
            stage_hidden: settings.call_stage_hidden && !settings.call_chat_hidden,
            video_only: settings.call_video_only,
            show_fps: settings.call_show_fps,
            nerd_stats: settings.call_nerd_stats,
        }
    }
}

/// A stored split, or the default when it is missing or broken.
pub fn ratio(stored: f32) -> f32 {
    if stored.is_finite() {
        stored.clamp(*SPLIT_RANGE.start(), *SPLIT_RANGE.end())
    } else {
        SPLIT
    }
}

/// Where the call, the divider and the chat go in `area`; `None` for what is hidden.
pub fn split(area: Rect, split: &Split) -> (Option<Rect>, Option<Rect>, Option<Rect>) {
    if split.stage_hidden {
        return (None, None, Some(area));
    }
    if split.chat_hidden {
        return (Some(area), None, None);
    }
    let ratio = ratio(split.ratio);
    let half = DIVIDER / 2.0;
    if split.side_by_side {
        let x = area.left() + area.width() * ratio;
        (
            Some(Rect::from_min_max(area.min, pos2(x - half, area.bottom()))),
            Some(Rect::from_x_y_ranges(x - half..=x + half, area.y_range())),
            Some(Rect::from_min_max(pos2(x + half, area.top()), area.max)),
        )
    } else {
        let y = area.top() + area.height() * ratio;
        (
            Some(Rect::from_min_max(area.min, pos2(area.right(), y - half))),
            Some(Rect::from_x_y_ranges(area.x_range(), y - half..=y + half)),
            Some(Rect::from_min_max(pos2(area.left(), y + half), area.max)),
        )
    }
}

pub struct Layout {
    pub channels: f32,
    pub members: f32,
    /// The person's choice for windows wide enough for the member list.
    members_open: bool,
    /// Opened by hand in a narrow window; forgotten when the window size class changes.
    peek: bool,
    narrow: bool,
    /// Phone-sized windows: the server and channel list fills the screen instead of
    /// what is open. Opening a channel, DM or page from the list clears it.
    pub list: bool,
    /// The window is phone-sized, as of the start of this frame.
    pub phone: bool,
    /// The voice channel split; `split_dragging` while its divider is held.
    pub split: Split,
    pub split_dragging: bool,
    /// What the settings file holds, so it is written only after a change.
    saved: Saved,
}

impl Layout {
    pub fn new(settings: &Settings) -> Self {
        let saved = Saved {
            channels: width(settings.channels_width, CHANNELS_WIDTH, CHANNELS_RANGE),
            members: width(settings.members_width, MEMBERS_WIDTH, MEMBERS_RANGE),
            members_open: !settings.members_hidden,
            split: Split::new(settings),
        };
        Self {
            channels: saved.channels,
            members: saved.members,
            members_open: saved.members_open,
            peek: false,
            narrow: false,
            list: true,
            phone: false,
            split: saved.split,
            split_dragging: false,
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

    pub fn toggle_members(&mut self) {
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
            split: self.split,
        };
        if now == self.saved {
            return;
        }
        self.saved = now;
        let mut settings = audio.settings();
        settings.channels_width = now.channels;
        settings.members_width = now.members;
        settings.members_hidden = !now.members_open;
        settings.call_side_by_side = now.split.side_by_side;
        settings.call_split = now.split.ratio;
        settings.call_chat_hidden = now.split.chat_hidden;
        settings.call_stage_hidden = now.split.stage_hidden;
        settings.call_video_only = now.split.video_only;
        settings.call_show_fps = now.split.show_fps;
        settings.call_nerd_stats = now.split.nerd_stats;
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
        t!("layout.hide_members")
    } else {
        t!("layout.show_members")
    };
    // A soft fill while the list is open, like a pressed toggle.
    let look = if shown { Look::Plain } else { Look::Ghost };
    if ui::icon_button(ui, "👥", tip, look, Vec2::new(30.0, 26.0)).clicked() {
        layout.toggle_members();
    }
}

/// The voice channel header buttons: chat below or beside the call, hide the chat,
/// hide the call. Hiding one always shows the other.
pub fn split_buttons(ui: &mut Ui, split: &mut Split) {
    let (glyph, tip) = if split.side_by_side {
        ("⬍", t!("layout.chat_below"))
    } else {
        ("⬌", t!("layout.chat_beside"))
    };
    let both = !split.chat_hidden && !split.stage_hidden;
    if ui
        .add_enabled_ui(both, |ui| {
            ui::icon_button(ui, glyph, tip, Look::Ghost, SPLIT_BUTTON)
        })
        .inner
        .clicked()
    {
        split.side_by_side ^= true;
    }
    let tip = if split.chat_hidden {
        t!("layout.show_chat")
    } else {
        t!("layout.hide_chat")
    };
    let look = if split.chat_hidden {
        Look::Ghost
    } else {
        Look::Plain
    };
    if ui::icon_button(ui, "💬", tip, look, SPLIT_BUTTON).clicked() {
        split.chat_hidden ^= true;
        split.stage_hidden = false;
    }
    let tip = if split.stage_hidden {
        t!("layout.show_call")
    } else {
        t!("layout.hide_call")
    };
    let look = if split.stage_hidden {
        Look::Ghost
    } else {
        Look::Plain
    };
    if ui::icon_button(ui, "📺", tip, look, SPLIT_BUTTON).clicked() {
        split.stage_hidden ^= true;
        split.chat_hidden = false;
    }
}

/// The handle between call and chat in `area`. Double-click resets the split;
/// returns whether it is being dragged.
pub fn split_divider(ui: &Ui, divider: Rect, area: Rect, split: &mut Split) -> bool {
    let response = ui.interact(divider, Id::unique("call split"), Sense::click_and_drag());
    response.widget_info(|| WidgetInfo::labeled(Role::Splitter, true, t!("layout.resize_call")));
    if response.double_clicked() {
        split.ratio = SPLIT;
    } else if response.dragged()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let wanted = if split.side_by_side {
            (pointer.x - area.left()) / area.width()
        } else {
            (pointer.y - area.top()) / area.height()
        };
        split.ratio = ratio(wanted);
    }
    let active = response.hovered() || response.dragged();
    let (cursor, line) = if split.side_by_side {
        let x = divider.center().x;
        let y = divider.center().y;
        (
            egui::CursorIcon::ResizeHorizontal,
            [pos2(x, y - 16.0), pos2(x, y + 16.0)],
        )
    } else {
        let (x, y) = (divider.center().x, divider.center().y);
        (
            egui::CursorIcon::ResizeVertical,
            [pos2(x - 16.0, y), pos2(x + 16.0, y)],
        )
    };
    // A short grip, accent while held.
    let color = if active {
        ui::accent(ui).base
    } else {
        ui::colors(ui).border
    };
    ui.painter().line_segment(line, Stroke::new(4.0, color));
    if active {
        ui.ctx().set_cursor_icon(cursor);
    }
    response.dragged()
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
    fn broken_split_falls_back_and_is_clamped() {
        let split = |value, chat_hidden, stage_hidden| {
            let settings = Settings {
                call_split: value,
                call_chat_hidden: chat_hidden,
                call_stage_hidden: stage_hidden,
                ..Settings::default()
            };
            Layout::new(&settings).split
        };
        assert_eq!(split(f32::NAN, false, false).ratio, SPLIT);
        assert_eq!(split(f32::INFINITY, false, false).ratio, SPLIT);
        assert_eq!(split(0.0, false, false).ratio, *SPLIT_RANGE.start());
        assert_eq!(split(5.0, false, false).ratio, *SPLIT_RANGE.end());
        assert_eq!(split(0.4, false, false).ratio, 0.4);
        // Both hidden would leave an empty pane: the call shows.
        let both = split(0.4, true, true);
        assert!(both.chat_hidden && !both.stage_hidden);
        // An older settings file without these keys gets the defaults.
        let old: Settings = serde_json::from_str("{}").unwrap();
        let split = Layout::new(&old).split;
        assert_eq!(split.ratio, SPLIT);
        assert!(!split.side_by_side && !split.chat_hidden && !split.stage_hidden);
        assert!(!split.video_only);
    }

    #[test]
    fn split_divides_the_area() {
        let area = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(1000.0, 800.0));
        let mut s = Layout::new(&Settings::default()).split;
        s.ratio = 0.5;
        let (stage, divider, chat) = split(area, &s);
        let (stage, divider, chat) = (stage.unwrap(), divider.unwrap(), chat.unwrap());
        assert_eq!(stage.bottom(), 400.0 - DIVIDER / 2.0);
        assert_eq!(chat.top(), 400.0 + DIVIDER / 2.0);
        assert_eq!(divider.height(), DIVIDER);
        assert_eq!(stage.width(), 1000.0);
        s.side_by_side = true;
        let (stage, _, chat) = split(area, &s);
        assert_eq!(stage.unwrap().right(), 500.0 - DIVIDER / 2.0);
        assert_eq!(chat.unwrap().height(), 800.0);
        // A ratio past the range is clamped here too.
        s.ratio = 0.99;
        assert_eq!(split(area, &s).0.unwrap().right(), 800.0 - DIVIDER / 2.0);
        s.chat_hidden = true;
        assert_eq!(split(area, &s), (Some(area), None, None));
        s.chat_hidden = false;
        s.stage_hidden = true;
        assert_eq!(split(area, &s), (None, None, Some(area)));
    }

    #[test]
    fn split_survives_the_settings_file() {
        let settings = Settings {
            call_side_by_side: true,
            call_split: 0.3,
            call_chat_hidden: true,
            call_video_only: true,
            call_show_fps: true,
            ..Settings::default()
        };
        let saved = serde_json::to_string(&settings).unwrap();
        let loaded: Settings = serde_json::from_str(&saved).unwrap();
        let split = Layout::new(&loaded).split;
        assert!(split.side_by_side && split.chat_hidden && split.video_only);
        assert!(split.show_fps && !split.nerd_stats);
        assert!(!split.stage_hidden);
        assert_eq!(split.ratio, 0.3);
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
