//! Small reusable pieces for the voice panel and the user bar: look constants,
//! an animated icon button, a fading card, a popup and a line chart.

use eframe::egui::{
    self, Align, Align2, Color32, CursorIcon, FontId, Frame, Layout, Margin, Popup,
    PopupCloseBehavior, Pos2, Rect, RectAlign, Response, Role, Sense, Shape, Stroke, Ui, UiBuilder,
    Vec2, WidgetInfo,
};

pub const GREEN: Color32 = Color32::from_rgb(0x23, 0xa5, 0x5a);
pub const AMBER: Color32 = Color32::from_rgb(0xf0, 0xb2, 0x32);
pub const RED: Color32 = Color32::from_rgb(0xda, 0x37, 0x3c);
pub const GREY: Color32 = Color32::from_rgb(0x80, 0x84, 0x8e);
/// A share that is live: camera, screen or app audio.
pub const LIVE: Color32 = GREEN;
pub const ICON_SIZE: f32 = 16.0;
pub const BUTTON_HEIGHT: f32 = 32.0;
pub const GAP: f32 = 4.0;
pub const RADIUS: u8 = 8;
pub const POPUP_WIDTH: f32 = 300.0;
/// Hover and press feedback.
pub const HOVER_SECONDS: f32 = 0.12;
/// A button turning on or off.
pub const STATE_SECONDS: f32 = 0.18;
/// A card appearing.
pub const CARD_SECONDS: f32 = 0.2;
/// How far a card slides up while it appears.
pub const CARD_SLIDE: f32 = 8.0;
/// Glyph growth on hover and shrink while pressed.
pub const HOVER_GROW: f32 = 0.1;
pub const PRESS_SHRINK: f32 = 0.12;

/// How an icon button looks when it is not just hovered.
#[derive(Clone, Copy, PartialEq)]
pub enum Look {
    Plain,
    /// Filled with this colour, white glyph: something is on.
    On(Color32),
    /// Red, white glyph: leaving.
    Danger,
    /// Red strike through the glyph: mic or sound is off.
    Struck,
}

/// A square-ish icon button with a fading hover background, a glyph that grows on
/// hover and shrinks while pressed, a short tooltip and `tip` as its accessible name.
/// Wrap it in `add_enabled_ui` to disable it.
pub fn icon_button(ui: &mut Ui, glyph: &str, tip: &str, look: Look, size: Vec2) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let ctx = ui.ctx().clone();
    let id = response.id;
    let hover = ctx.animate_bool_with_time(id.with("hover"), response.hovered(), HOVER_SECONDS);
    let press = ctx.animate_bool_with_time(
        id.with("press"),
        response.is_pointer_button_down_on(),
        HOVER_SECONDS,
    );
    let (color, on) = match look {
        Look::Plain => (Color32::TRANSPARENT, false),
        Look::On(color) => (color, true),
        Look::Danger => (RED, true),
        Look::Struck => (RED.gamma_multiply(0.22), true),
    };
    let on_t = ctx.animate_bool_with_time(id.with("on"), on, STATE_SECONDS);
    let visuals = ui.visuals();
    let idle = visuals.widgets.inactive.weak_bg_fill.gamma_multiply(0.6);
    let fill = idle
        .lerp_to_gamma(visuals.widgets.hovered.weak_bg_fill, hover)
        .lerp_to_gamma(color, on_t)
        .lerp_to_gamma(Color32::WHITE, 0.12 * hover * on_t);
    let white = matches!(look, Look::On(_) | Look::Danger);
    let glyph_color = visuals
        .widgets
        .inactive
        .fg_stroke
        .color
        .lerp_to_gamma(visuals.strong_text_color(), hover)
        .lerp_to_gamma(Color32::WHITE, if white { on_t } else { 0.0 });
    let painter = ui.painter();
    painter.rect_filled(rect, RADIUS, fill);
    let scale = 1.0 + HOVER_GROW * hover - PRESS_SHRINK * press;
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::proportional(ICON_SIZE * scale),
        glyph_color,
    );
    if look == Look::Struck {
        // The strike grows in with the state.
        let half = Vec2::splat(ICON_SIZE * 0.55 * on_t);
        let center = rect.center();
        painter.line_segment([center - half, center + half], Stroke::new(2.0, RED));
    }
    response.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), tip));
    response
        .on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// A rounded card that fades and slides in while `shown`; `None` when hidden.
pub fn card<R>(
    ui: &mut Ui,
    id: &'static str,
    shown: bool,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let t = ui
        .ctx()
        .animate_bool_with_time(egui::Id::unique(id), shown, CARD_SECONDS);
    if !shown {
        return None;
    }
    let fill = ui.visuals().extreme_bg_color;
    let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
    let inner = ui.scope(|ui| {
        ui.multiply_opacity(t);
        ui.add_space((1.0 - t) * CARD_SLIDE);
        Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(RADIUS)
            .inner_margin(Margin::same(8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                add(ui)
            })
            .inner
    });
    Some(inner.inner)
}

/// A popup above `anchor` that opens on click and closes on a click outside it.
pub fn popup<R>(anchor: &Response, add: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    Popup::menu(anchor)
        .align(RectAlign::TOP_START)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .width(POPUP_WIDTH)
        .show(|ui| {
            ui.set_width(POPUP_WIDTH);
            add(ui)
        })
        .map(|inner| inner.inner)
}

/// A small line chart of `values`, oldest first, filling `size` from the right
/// with `slots` points across. The top is the highest value plus some room.
pub fn line_chart(ui: &mut Ui, values: &[f64], slots: usize, size: Vec2, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4, ui.visuals().extreme_bg_color);
    let top = values.iter().copied().fold(50.0, f64::max) * 1.2;
    let step = rect.width() / slots.saturating_sub(1).max(1) as f32;
    let start = slots.saturating_sub(values.len()) as f32;
    let points: Vec<Pos2> = values
        .iter()
        .enumerate()
        .map(|(at, value)| {
            let y = rect.bottom() - (*value / top) as f32 * rect.height();
            Pos2::new(rect.left() + (start + at as f32) * step, y)
        })
        .collect();
    if let [only] = points[..] {
        painter.circle_filled(only, 2.0, color);
    } else if points.len() > 1 {
        painter.add(Shape::line(points, Stroke::new(1.5, color)));
    }
}

/// A coloured dot, for status and connection quality.
pub fn dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

/// A full-width clickable row `height` tall holding `add`, with the same hover
/// fade as the icon buttons and `label` as its accessible name.
pub fn row_button(ui: &mut Ui, height: f32, label: &str, add: impl FnOnce(&mut Ui)) -> Response {
    let rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), height));
    let background = ui.painter().add(Shape::Noop);
    let inner = rect.shrink2(Vec2::new(6.0, 2.0));
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Center)),
    );
    add(&mut child);
    // Allocated after the contents, so it takes the clicks on top of them.
    let response = ui.allocate_rect(rect, Sense::click());
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered(),
        HOVER_SECONDS,
    );
    let fill = Color32::TRANSPARENT.lerp_to_gamma(ui.visuals().widgets.hovered.weak_bg_fill, hover);
    ui.painter()
        .set(background, Shape::rect_filled(rect, RADIUS, fill));
    response.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), label));
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// A full-width row `height` tall laid out from the right, so trailing buttons
/// come first and the rest fills what is left.
pub fn right_row<R>(ui: &mut Ui, height: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let size = Vec2::new(ui.available_width(), height);
    ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), add)
        .inner
}
