//! Pieces of long, sectioned pages like Settings: section titles the sidebar can
//! jump to, label-and-control rows, a toggle, info banners and a danger zone.

use eframe::egui::{
    self, Align, Color32, CursorIcon, FontId, Frame, Id, Label, Layout, Margin, Rect, Response,
    RichText, Sense, Stroke, Ui, UiBuilder, Vec2, WidgetInfo, accesskit::Role,
};

use super::{
    CONTROL_HEIGHT, HOVER_SECONDS, ON_FILL, RADIUS, RADIUS_ROW, SPACE_L, SPACE_M, SPACE_S,
    SPACE_XL, SPACE_XS, STATE_SECONDS, TEXT_BODY, TEXT_SMALL, accent, bold, bold_family, colors,
    focus_ring, small_close,
};

/// Section titles in a page: bigger than pane titles, smaller than page headings.
pub const TEXT_SECTION: f32 = 19.0;
/// Height of a section link under the open page in a sidebar.
const SUB_ROW: f32 = 28.0;
/// Where the guide line of section links sits, under the page icon.
const GUIDE_X: f32 = 19.0;
/// Where section link text starts, in line with the page labels.
const SUB_TEXT_X: f32 = 34.0;
const TOGGLE: Vec2 = Vec2::new(40.0, 22.0);
/// The size of `small_close`.
const CLOSE: f32 = 22.0;
/// Room above and below the content of a `setting_row`.
const ROW_PAD: f32 = SPACE_XS;

/// The sections of the page being drawn, collected while a sectioned page draws.
#[derive(Clone, Default)]
pub struct Sections {
    pub titles: Vec<String>,
    /// The top of each section on screen, divider included.
    pub tops: Vec<f32>,
    /// The section to scroll to while drawing.
    pub jump: Option<usize>,
}

fn sections_id() -> Id {
    Id::unique("rusttalk page sections")
}

/// Collect the sections `add` draws with `page_section`, scrolling to `jump`.
pub fn collect_sections<R>(
    ui: &mut Ui,
    jump: Option<usize>,
    add: impl FnOnce(&mut Ui) -> R,
) -> (R, Sections) {
    let collecting = Sections {
        jump,
        ..Sections::default()
    };
    ui.data_mut(|data| data.insert_temp(sections_id(), collecting));
    let inner = add(ui);
    let sections = ui
        .data_mut(|data| data.remove_temp(sections_id()))
        .unwrap_or_default();
    (inner, sections)
}

/// A large section title with a divider above it, except for a page's first section.
/// In a page drawn through `collect_sections` the sidebar lists it and can scroll here.
pub fn page_section(ui: &mut Ui, title: &str) -> Response {
    let mut sections: Option<Sections> = ui.data(|data| data.get_temp(sections_id()));
    let first = sections.as_ref().map_or_else(
        || ui.cursor().top() <= ui.max_rect().top() + 1.0,
        |s| s.titles.is_empty(),
    );
    let top = ui.cursor().top();
    if first {
        ui.add_space(SPACE_S);
    } else {
        ui.add_space(SPACE_XL);
        let y = ui.cursor().top();
        let x = ui.max_rect().x_range();
        ui.painter()
            .hline(x, y, Stroke::new(1.0, colors(ui).border));
        ui.add_space(SPACE_XL);
    }
    let text = RichText::new(title)
        .family(bold_family())
        .size(TEXT_SECTION)
        .color(colors(ui).strong);
    let response = ui.add(Label::new(text).selectable(false));
    ui.add_space(SPACE_S);
    if let Some(s) = &mut sections {
        if s.jump == Some(s.titles.len()) {
            ui.scroll_to_rect(response.rect.expand(SPACE_L), Some(Align::TOP));
        }
        s.titles.push(title.to_owned());
        s.tops.push(top);
        ui.data_mut(|data| data.insert_temp(sections_id(), s.clone()));
    }
    response
}

/// The section `view_top` is in: the last one whose top passed it, or the last one when
/// scrolled to the end, so short last sections can be reached.
pub fn current_section(tops: &[f32], view_top: f32, at_end: bool) -> usize {
    if at_end {
        return tops.len().saturating_sub(1);
    }
    tops.iter()
        .rposition(|top| *top <= view_top + SPACE_XL)
        .unwrap_or(0)
}

/// A settings row: `label` with a line of `help` on the left, `control` on the right.
/// Sizes come from the last frame, so a size change redraws the frame unseen.
pub fn setting_row<R>(
    ui: &mut Ui,
    label: &str,
    help: &str,
    control: impl FnOnce(&mut Ui) -> R,
) -> R {
    let id = ui.scope_id().with(("setting row", label));
    let (control_size, text_height): (Vec2, f32) =
        ui.data(|data| data.get_temp(id)).unwrap_or_default();
    let width = ui.available_width();
    let height = control_size.y.max(text_height).max(CONTROL_HEIGHT);
    let min = ui.cursor().min + Vec2::new(0.0, ROW_PAD);
    let text_width = (width - control_size.x - SPACE_L).max(width * 0.4);

    let text_top = min.y + (height - text_height) / 2.0;
    let mut text = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_size(
                egui::pos2(min.x, text_top),
                Vec2::new(text_width, height),
            ))
            .layout(Layout::top_down(Align::Min)),
    );
    text.spacing_mut().item_spacing.y = 2.0;
    text.add(Label::new(bold(label).color(colors(ui).strong)).wrap());
    if !help.is_empty() {
        let help = RichText::new(help).size(TEXT_SMALL).color(colors(ui).muted);
        text.add(Label::new(help).wrap());
    }
    let new_text = text.min_rect().height();

    let control_top = min.y + (height - control_size.y) / 2.0;
    let mut controls = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                // Until measured, the control gets all the room the text leaves.
                egui::pos2(
                    if control_size.x > 0.0 {
                        min.x + width - control_size.x
                    } else {
                        min.x + width * 0.4 + SPACE_L
                    },
                    control_top,
                ),
                egui::pos2(
                    min.x + width,
                    control_top + control_size.y.max(CONTROL_HEIGHT),
                ),
            ))
            .layout(Layout::left_to_right(Align::Center)),
    );
    let inner = control(&mut controls);
    let new_control = if controls.min_rect().width() > 0.5 {
        controls.min_rect().size()
    } else {
        Vec2::ZERO
    };

    let changed =
        (new_control - control_size).length() > 0.5 || (new_text - text_height).abs() > 0.5;
    if changed {
        ui.data_mut(|data| data.insert_temp(id, (new_control, new_text)));
        ui.ctx().request_discard("settings row size");
    }
    let height = new_control.y.max(new_text).max(CONTROL_HEIGHT);
    ui.allocate_rect(
        Rect::from_min_size(
            min - Vec2::new(0.0, ROW_PAD),
            Vec2::new(width, height + 2.0 * ROW_PAD),
        ),
        Sense::hover(),
    );
    inner
}

/// A `setting_row` with a toggle.
pub fn toggle_row(ui: &mut Ui, on: &mut bool, label: &str, help: &str) -> Response {
    setting_row(ui, label, help, |ui| toggle(ui, on, label))
}

/// An on/off switch that slides and fills with the accent; `label` is its accessible name.
pub fn toggle(ui: &mut Ui, on: &mut bool, label: &str) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(TOGGLE, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let value = *on;
    response.widget_info(|| WidgetInfo::selected(Role::Switch, ui.is_enabled(), value, label));
    let c = colors(ui);
    let t = ui
        .ctx()
        .animate_bool_with_time(response.id.with("on"), value, STATE_SECONDS);
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered(),
        HOVER_SECONDS,
    );
    let off = c.offline.lerp_to_gamma(c.muted, 0.5 * hover);
    let fill = off.lerp_to_gamma(accent(ui).base, t);
    let radius = rect.height() / 2.0;
    let painter = ui.painter();
    painter.rect_filled(rect, radius, fill);
    let knob = egui::pos2(
        egui::lerp(rect.left() + radius..=rect.right() - radius, t),
        rect.center().y,
    );
    painter.circle_filled(knob, radius - 3.0, ON_FILL);
    focus_ring(ui, &response, radius as u8);
    response.on_hover_cursor(CursorIcon::PointingHand)
}

fn dismissed_id(id: &str) -> Id {
    Id::unique(("rusttalk banner", id))
}

/// A tinted info line with an ℹ and a × that hides it until RustTalk restarts.
/// `id` tells banners apart.
pub fn banner(ui: &mut Ui, id: &str, text: &str) {
    let key = dismissed_id(id);
    if ui.data(|data| data.get_temp::<bool>(key)).unwrap_or(false) {
        return;
    }
    let tint = accent(ui).text(ui.visuals().dark_mode);
    let mut close = false;
    Frame::new()
        .fill(accent(ui).base.gamma_multiply(0.14))
        .stroke(Stroke::new(1.0, accent(ui).base.gamma_multiply(0.6)))
        .corner_radius(RADIUS)
        .inner_margin(Margin::symmetric(SPACE_M as i8, SPACE_S as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("ℹ").color(tint).size(TEXT_BODY + 1.0));
                let room = ui.available_width() - CLOSE - SPACE_S;
                ui.scope(|ui| {
                    ui.set_max_width(room);
                    ui.add(Label::new(RichText::new(text).color(colors(ui).text)).wrap());
                });
                ui.add_space(ui.available_width() - CLOSE);
                close = small_close(ui, "Dismiss").clicked();
            });
        });
    ui.add_space(SPACE_S);
    if close {
        ui.data_mut(|data| data.insert_temp(key, true));
    }
}

/// The last section of a page: things that can't be undone, in a red outline.
pub fn danger_zone<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    page_section(ui, "Danger zone");
    let c = colors(ui);
    Frame::new()
        .stroke(Stroke::new(1.0, c.danger))
        .corner_radius(RADIUS)
        .inner_margin(Margin::same(SPACE_L as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A section link under the open page in a sidebar, indented on a guide line; the
/// current one is highlighted and marks its part of the line in the accent.
pub fn nav_sub_row(ui: &mut Ui, label: &str, current: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(0.0), SUB_ROW),
        Sense::click(),
    );
    let c = colors(ui);
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered(),
        HOVER_SECONDS,
    );
    let painter = ui.painter();
    let row = Rect::from_min_max(
        egui::pos2(rect.left() + SUB_TEXT_X - 8.0, rect.top()),
        rect.max,
    );
    painter.rect_filled(
        row,
        RADIUS_ROW,
        Color32::TRANSPARENT.lerp_to_gamma(c.hover_row(), hover),
    );
    let x = rect.left() + GUIDE_X;
    let line = if current {
        Stroke::new(2.0, accent(ui).text(c.dark))
    } else {
        Stroke::new(1.0, c.border)
    };
    painter.vline(x, rect.y_range(), line);
    let color = if current {
        c.strong
    } else {
        c.muted.lerp_to_gamma(c.strong, hover)
    };
    let font = if current {
        FontId::new(TEXT_SMALL + 1.0, bold_family())
    } else {
        FontId::proportional(TEXT_SMALL + 1.0)
    };
    let galley = painter.layout(
        label.to_owned(),
        font,
        color,
        (rect.right() - rect.left() - SUB_TEXT_X).max(0.0),
    );
    let at = egui::pos2(
        rect.left() + SUB_TEXT_X,
        rect.center().y - galley.size().y / 2.0,
    );
    painter.with_clip_rect(rect).galley(at, galley, color);
    focus_ring(ui, &response, RADIUS_ROW);
    response.widget_info(|| WidgetInfo::selected(Role::Link, true, current, label));
    response.on_hover_cursor(CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_section_whose_top_passed_is_current() {
        let tops = [100.0, 400.0, 900.0];
        assert_eq!(current_section(&tops, 0.0, false), 0);
        assert_eq!(current_section(&tops, 390.0, false), 1);
        assert_eq!(current_section(&tops, 1000.0, false), 2);
        assert_eq!(current_section(&tops, 390.0, true), 2);
        assert_eq!(current_section(&[], 0.0, true), 0);
    }
}
