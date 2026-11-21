//! The shared widgets: buttons, fields, checkbox, icon button, cards, rows, dialogs,
//! badges and the appearance pickers. They read every colour from the active theme.

use eframe::egui::{
    self, Align, Align2, Area, Button, Color32, Context, CornerRadius, CursorIcon, FontId, Frame,
    Id, Layout, Margin, Modal, Order, Popup, PopupCloseBehavior, PopupKind, Pos2, Rect, RectAlign,
    Response, RichText, Sense, Shape, Stroke, StrokeKind, TextBuffer, TextEdit, Ui, UiBuilder,
    Vec2, Widget, WidgetInfo, WidgetText, accesskit::Role,
};

use super::{
    Accent, CARD_SECONDS, CARD_SLIDE, CONTROL_HEIGHT, DIALOG_PADDING, DIALOG_WIDTH, HOVER_GROW,
    HOVER_SECONDS, LARGE_CONTROL, ON_FILL, POPUP_WIDTH, PRESS_SHRINK, Palette, RADIUS, RADIUS_ROW,
    RADIUS_S, RADIUS_WINDOW, SPACE_L, SPACE_M, SPACE_S, STATE_SECONDS, TEXT_BODY, TEXT_CAPTION,
    TEXT_ICON, TEXT_SMALL, accent, bold, bold_family, colors, heading,
};

/// How an icon button looks when it is not just hovered.
#[derive(Clone, Copy, PartialEq)]
pub enum Look {
    /// A soft button fill.
    Plain,
    /// No fill until hovered: in toolbars and next to text.
    Ghost,
    /// Filled with this colour, white glyph: something is on.
    On(Color32),
    /// Red, white glyph: leaving.
    Danger,
    /// Red strike through the glyph: mic or sound is off.
    Struck,
}

/// A square-ish icon button with a fading hover background, a glyph that grows on
/// hover and shrinks while pressed, a tooltip and `tip` as its accessible name.
/// Wrap it in `add_enabled_ui` to disable it.
pub fn icon_button(ui: &mut Ui, glyph: &str, tip: &str, look: Look, size: Vec2) -> Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let ctx = ui.ctx().clone();
    let c = colors(ui);
    let id = response.id;
    let hover = ctx.animate_bool_with_time(id.with("hover"), response.hovered(), HOVER_SECONDS);
    let press = ctx.animate_bool_with_time(
        id.with("press"),
        response.is_pointer_button_down_on(),
        HOVER_SECONDS,
    );
    let (color, on) = match look {
        Look::Plain | Look::Ghost => (Color32::TRANSPARENT, false),
        Look::On(color) => (color, true),
        Look::Danger => (c.danger, true),
        Look::Struck => (c.danger.gamma_multiply(0.22), true),
    };
    let on_t = ctx.animate_bool_with_time(id.with("on"), on, STATE_SECONDS);
    let idle = if look == Look::Ghost {
        Color32::TRANSPARENT
    } else {
        c.button.gamma_multiply(0.45)
    };
    let fill = idle
        .lerp_to_gamma(c.hover, hover)
        .lerp_to_gamma(color, on_t)
        .lerp_to_gamma(Color32::WHITE, 0.12 * hover * on_t);
    let white = matches!(look, Look::On(_) | Look::Danger);
    let glyph_color = c
        .muted
        .lerp_to_gamma(c.strong, hover)
        .lerp_to_gamma(ON_FILL, if white { on_t } else { 0.0 });
    let painter = ui.painter();
    painter.rect_filled(rect, RADIUS, fill);
    focus_ring(ui, &response, RADIUS);
    let scale = 1.0 + HOVER_GROW * hover - PRESS_SHRINK * press;
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::proportional(TEXT_ICON * scale),
        glyph_color,
    );
    if look == Look::Struck {
        // The strike grows in with the state.
        let half = Vec2::splat(TEXT_ICON * 0.55 * on_t);
        let center = rect.center();
        painter.line_segment(
            [center - half, center + half],
            Stroke::new(2.0, c.danger_text),
        );
    }
    response.widget_info(|| WidgetInfo::labeled(Role::Button, ui.is_enabled(), tip));
    response
        .on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// The accent outline of a widget that has keyboard focus.
pub fn focus_ring(ui: &Ui, response: &Response, radius: u8) {
    if response.has_focus() {
        let stroke = Stroke::new(1.5, accent(ui).text(ui.visuals().dark_mode));
        ui.painter()
            .rect_stroke(response.rect, radius, stroke, StrokeKind::Inside);
    }
}

enum Kind {
    Primary,
    Secondary,
    Danger,
}

fn button(ui: &mut Ui, text: &str, kind: Kind, size: Vec2, text_size: f32) -> Response {
    let c = colors(ui);
    let a = accent(ui);
    let fills = match kind {
        Kind::Primary => Some((a.base, a.strong, a.strong)),
        Kind::Danger => Some((
            c.danger,
            c.danger.gamma_multiply(0.85),
            c.danger.gamma_multiply(0.75),
        )),
        Kind::Secondary => None,
    };
    ui.scope(|ui| {
        if let Some((idle, hover, pressed)) = fills {
            let widgets = &mut ui.visuals_mut().widgets;
            for (state, fill) in [
                (&mut widgets.inactive, idle),
                (&mut widgets.hovered, hover),
                (&mut widgets.active, pressed),
            ] {
                state.weak_bg_fill = fill;
                state.fg_stroke.color = ON_FILL;
            }
        }
        ui.add(
            Button::new(RichText::new(text).family(bold_family()).size(text_size))
                .min_size(size)
                .corner_radius(RADIUS),
        )
        .on_hover_cursor(CursorIcon::PointingHand)
    })
    .inner
}

const NORMAL: Vec2 = Vec2::new(0.0, CONTROL_HEIGHT);

/// The main action: accent fill, white text.
pub fn primary(ui: &mut Ui, text: &str) -> Response {
    button(ui, text, Kind::Primary, NORMAL, TEXT_BODY)
}

/// Any other action.
pub fn secondary(ui: &mut Ui, text: &str) -> Response {
    button(ui, text, Kind::Secondary, NORMAL, TEXT_BODY)
}

/// A toggle in a compact picker.
pub fn choice_button<'a>(
    ui: &mut Ui,
    selected: bool,
    label: impl egui::IntoAtoms<'a>,
    size: Vec2,
) -> Response {
    ui.add(Button::selectable(selected, label).min_size(size))
}

/// Something that can't be undone, like Delete, Kick or Leave.
pub fn danger(ui: &mut Ui, text: &str) -> Response {
    button(ui, text, Kind::Danger, NORMAL, TEXT_BODY)
}

/// The big buttons of the sign-up screens.
pub fn primary_large(ui: &mut Ui, text: &str) -> Response {
    button(ui, text, Kind::Primary, LARGE_CONTROL, 16.0)
}

pub fn secondary_large(ui: &mut Ui, text: &str) -> Response {
    button(ui, text, Kind::Secondary, LARGE_CONTROL, 16.0)
}

/// A text field with roomy padding and an accent outline while typing in it.
pub struct Field<'t> {
    edit: TextEdit<'t>,
}

/// A one-line text field.
pub fn field(text: &mut dyn TextBuffer) -> Field<'_> {
    Field {
        edit: TextEdit::singleline(text).margin(Margin::symmetric(10, 7)),
    }
}

/// A text field of several lines.
pub fn text_area(text: &mut dyn TextBuffer) -> Field<'_> {
    Field {
        edit: TextEdit::multiline(text).margin(Margin::symmetric(10, 7)),
    }
}

impl Field<'_> {
    pub fn hint_text(mut self, hint: impl Into<WidgetText>) -> Self {
        self.edit = self.edit.hint_text(hint);
        self
    }

    pub fn desired_width(mut self, width: f32) -> Self {
        self.edit = self.edit.desired_width(width);
        self
    }

    pub fn desired_rows(mut self, rows: usize) -> Self {
        self.edit = self.edit.desired_rows(rows);
        self
    }

    pub fn char_limit(mut self, limit: usize) -> Self {
        self.edit = self.edit.char_limit(limit);
        self
    }
}

impl Widget for Field<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let response = ui.add(self.edit);
        field_focus(ui, &response);
        response
    }
}

/// The accent outline of a focused text field.
pub fn field_focus(ui: &Ui, response: &Response) {
    if response.has_focus() {
        let stroke = Stroke::new(1.5, accent(ui).text(ui.visuals().dark_mode));
        ui.painter()
            .rect_stroke(response.rect, RADIUS, stroke, StrokeKind::Inside);
    }
}

/// A problem, in the theme's danger colour.
pub fn error_text(ui: &mut Ui, error: Option<&str>) {
    if let Some(error) = error {
        ui.colored_label(colors(ui).danger_text, error);
    }
}

/// A checkbox with a rounded box that fills with the accent when on.
pub struct Checkbox<'a> {
    checked: &'a mut bool,
    text: WidgetText,
}

impl<'a> Checkbox<'a> {
    pub fn new(checked: &'a mut bool, text: impl Into<WidgetText>) -> Self {
        Self {
            checked,
            text: text.into(),
        }
    }
}

const BOX: f32 = 18.0;

impl Widget for Checkbox<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        let Checkbox { checked, text } = self;
        let gap = ui.spacing().icon_spacing + 2.0;
        let wrap = ui.available_width() - BOX - gap;
        let galley = text.into_galley(ui, None, wrap, egui::TextStyle::Body);
        let label = !galley.is_empty();
        let width = BOX + if label { gap + galley.size().x } else { 0.0 };
        let height = galley.size().y.max(BOX).max(ui.spacing().interact_size.y);
        let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        if response.clicked() {
            *checked = !*checked;
            response.mark_changed();
        }
        let on = *checked;
        response.widget_info(|| {
            WidgetInfo::selected(Role::CheckBox, ui.is_enabled(), on, galley.text())
        });
        if ui.is_rect_visible(rect) {
            let c = colors(ui);
            let t = ui
                .ctx()
                .animate_bool_with_time(response.id.with("on"), on, STATE_SECONDS);
            let hovered = response.hovered();
            let mark = Rect::from_center_size(
                egui::pos2(rect.left() + BOX / 2.0, rect.center().y),
                Vec2::splat(BOX),
            );
            let idle = if hovered { c.hover } else { c.field };
            let fill = idle.lerp_to_gamma(accent(ui).base, t);
            let painter = ui.painter();
            painter.rect(
                mark,
                RADIUS_S,
                fill,
                Stroke::new(1.0, c.border.lerp_to_gamma(accent(ui).base, t)),
                StrokeKind::Inside,
            );
            if t > 0.0 {
                let points = [
                    mark.lerp_inside(Vec2::new(0.25, 0.52)),
                    mark.lerp_inside(Vec2::new(0.43, 0.7)),
                    mark.lerp_inside(Vec2::new(0.76, 0.32)),
                ];
                painter.add(Shape::line(
                    points.to_vec(),
                    Stroke::new(2.0, ON_FILL.gamma_multiply(t)),
                ));
            }
            if response.has_focus() {
                let stroke = Stroke::new(1.5, accent(ui).text(c.dark));
                painter.rect_stroke(mark.expand(2.0), RADIUS_S + 2, stroke, StrokeKind::Outside);
            }
            if label {
                let color = if hovered { c.strong } else { c.text };
                let at = egui::pos2(mark.right() + gap, rect.center().y - galley.size().y / 2.0);
                painter.galley(at, galley, color);
            }
        }
        response.on_hover_cursor(CursorIcon::PointingHand)
    }
}

pub fn checkbox(ui: &mut Ui, checked: &mut bool, text: impl Into<WidgetText>) -> Response {
    ui.add(Checkbox::new(checked, text))
}

/// One of a few choices side by side, like "720p | 1080p | Native", the picked one in
/// the accent and its hint (if any) under the row. Each choice is a radio button for
/// screen readers. Choices are (value, label, hint). True when changed.
pub fn segmented<T: Copy + PartialEq>(
    ui: &mut Ui,
    value: &mut T,
    choices: &[(T, &str, &str)],
) -> bool {
    let width = ui.available_width();
    let (rect, row) = ui.allocate_exact_size(Vec2::new(width, CONTROL_HEIGHT), Sense::hover());
    let c = colors(ui);
    let a = accent(ui);
    ui.painter().rect_filled(rect, RADIUS, c.field);
    let inner = rect.shrink(3.0);
    let step = inner.width() / choices.len().max(1) as f32;
    let mut changed = false;
    for (at, (choice, label, _)) in choices.iter().enumerate() {
        let cell = Rect::from_min_size(
            inner.min + Vec2::new(at as f32 * step, 0.0),
            Vec2::new(step, inner.height()),
        );
        let response = ui.interact(cell, row.id.with(at), Sense::click());
        if response.clicked() && *value != *choice {
            *value = *choice;
            changed = true;
        }
        let on = *value == *choice;
        let t = ui
            .ctx()
            .animate_bool_with_time(response.id.with("on"), on, STATE_SECONDS);
        let hover = ui.ctx().animate_bool_with_time(
            response.id.with("hover"),
            response.hovered() && !on,
            HOVER_SECONDS,
        );
        let fill = Color32::TRANSPARENT
            .lerp_to_gamma(c.hover, hover)
            .lerp_to_gamma(a.base, t);
        let text = c.text.lerp_to_gamma(ON_FILL, t);
        let painter = ui.painter();
        painter.rect_filled(cell, RADIUS_S + 2, fill);
        painter.text(
            cell.center(),
            Align2::CENTER_CENTER,
            *label,
            FontId::new(TEXT_SMALL, bold_family()),
            text,
        );
        focus_ring(ui, &response, RADIUS_S + 2);
        response
            .widget_info(|| WidgetInfo::selected(Role::RadioButton, ui.is_enabled(), on, *label));
        response.on_hover_cursor(CursorIcon::PointingHand);
    }
    if let Some((_, _, hint)) = choices.iter().find(|c| c.0 == *value)
        && !hint.is_empty()
    {
        ui.add_space(2.0);
        ui.label(RichText::new(*hint).size(TEXT_CAPTION).color(c.muted));
    }
    changed
}

/// A group header like "TEXT CHANNELS", with room above it.
pub fn section(ui: &mut Ui, text: &str) -> Response {
    ui.add_space(SPACE_S + 2.0);
    caption_label(ui, text)
}

/// Upper-case caption text, for group headers.
pub fn caption_label(ui: &mut Ui, text: &str) -> Response {
    let text = RichText::new(text.to_uppercase())
        .size(TEXT_CAPTION)
        .family(bold_family())
        .color(colors(ui).muted);
    ui.add(egui::Label::new(text).selectable(false))
}

/// A rounded box that sets a group of things apart.
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    card_frame(ui)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// The frame of `card`, for cards that size to their content.
pub fn card_frame(ui: &Ui) -> Frame {
    Frame::new()
        .fill(ui.visuals().extreme_bg_color)
        .corner_radius(RADIUS)
        .inner_margin(Margin::same(SPACE_L as i8))
}

/// A `card` that fades and slides in while `shown`; `None` when hidden.
pub fn fade_card<R>(
    ui: &mut Ui,
    id: &'static str,
    shown: bool,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let t = ui
        .ctx()
        .animate_bool_with_time(Id::unique(id), shown, CARD_SECONDS);
    if !shown {
        return None;
    }
    let c = colors(ui);
    let inner = ui.scope(|ui| {
        ui.multiply_opacity(t);
        ui.add_space((1.0 - t) * CARD_SLIDE);
        Frame::new()
            .fill(c.field)
            .stroke(Stroke::new(1.0, c.border))
            .corner_radius(RADIUS)
            .inner_margin(Margin::same(SPACE_S as i8))
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

/// A larger popup above the right end of `anchor`, open while `open` is set; a click
/// outside it or Escape clears it. The frame has no padding, so a rail or a bar can
/// run to its edges.
pub fn panel_popup<R>(
    ui: &Ui,
    id: Id,
    anchor: Rect,
    open: &mut bool,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let c = colors(ui);
    let frame = Frame::new()
        .fill(c.raised)
        .stroke(Stroke::new(1.0, c.border))
        .corner_radius(RADIUS_WINDOW)
        .shadow(ui.visuals().popup_shadow);
    Popup::new(id, ui.ctx().clone(), anchor, ui.layer_id())
        .kind(PopupKind::Popup)
        .open_bool(open)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .align(RectAlign::TOP_END)
        .gap(SPACE_S)
        .frame(frame)
        .show(add)
        .map(|inner| inner.inner)
}

/// A small line chart of `values`, oldest first, filling `size` from the right
/// with `slots` points across. The top is the highest value plus some room.
pub fn line_chart(ui: &mut Ui, values: &[f64], slots: usize, size: Vec2, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, RADIUS_S, colors(ui).field);
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

/// A presence dot on the bottom right of a picture, ringed in `ring` so it stands out.
pub fn presence_dot(ui: &Ui, picture: Rect, radius: f32, color: Color32, ring: Color32) {
    let center = picture.right_bottom() - Vec2::splat(radius * 0.8);
    ui.painter().circle(center, radius, color, (2.0, ring));
}

/// A full-width clickable row `height` tall holding `add`, with a fading hover and
/// `label` as its accessible name.
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
    let fill = Color32::TRANSPARENT.lerp_to_gamma(colors(ui).hover, hover);
    ui.painter()
        .set(background, Shape::rect_filled(rect, RADIUS_ROW, fill));
    focus_ring(ui, &response, RADIUS_ROW);
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

/// A list row with an icon and a label: a channel or a settings page. Muted until
/// hovered or `selected`; the open one gets the selected fill.
pub fn nav_row(
    ui: &mut Ui,
    icon: &str,
    label: &str,
    selected: bool,
    role: Role,
    accessible: &str,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width().max(0.0), super::ROW_HEIGHT),
        Sense::click(),
    );
    let c = colors(ui);
    let enabled = ui.is_enabled();
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered() && enabled,
        HOVER_SECONDS,
    );
    let fill = if selected {
        c.selected
    } else {
        Color32::TRANSPARENT.lerp_to_gamma(c.hover_row(), hover)
    };
    let text = if selected {
        c.strong
    } else {
        c.muted.lerp_to_gamma(c.strong, hover)
    };
    let painter = ui.painter();
    painter.rect_filled(rect, RADIUS_ROW, fill);
    focus_ring(ui, &response, RADIUS_ROW);
    let left = rect.left_center();
    painter.text(
        left + Vec2::new(10.0, 0.0),
        Align2::LEFT_CENTER,
        icon,
        FontId::proportional(TEXT_ICON),
        if selected { c.strong } else { c.muted },
    );
    let label_font = FontId::new(TEXT_BODY, crate::emoji::family(selected));
    let galley = painter.layout(
        label.to_owned(),
        label_font,
        text,
        (rect.width() - 44.0).max(0.0),
    );
    let at = egui::pos2(rect.left() + 34.0, rect.center().y - galley.size().y / 2.0);
    painter.with_clip_rect(rect).galley(at, galley, text);
    response.widget_info(|| WidgetInfo::selected(role, enabled, selected, accessible));
    response.on_hover_cursor(CursorIcon::PointingHand)
}

impl Palette {
    /// The hover fill of a row on this palette's side surface.
    pub fn hover_row(&self) -> Color32 {
        self.hover.lerp_to_gamma(self.side, 0.35)
    }
}

/// The round × of a window or dialog. True when clicked.
pub fn close_button(ui: &mut Ui, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let c = colors(ui);
    let hover = ui.ctx().animate_bool_with_time(
        response.id.with("hover"),
        response.hovered(),
        HOVER_SECONDS,
    );
    let color = c.muted.lerp_to_gamma(c.strong, hover);
    let painter = ui.painter();
    painter.circle(
        rect.center(),
        size / 2.0 - 1.0,
        Color32::TRANSPARENT.lerp_to_gamma(c.hover, hover),
        Stroke::new(1.5, color),
    );
    let half = size * 0.15;
    let center = rect.center();
    for (a, b) in [
        (Vec2::new(-half, -half), Vec2::new(half, half)),
        (Vec2::new(-half, half), Vec2::new(half, -half)),
    ] {
        painter.line_segment([center + a, center + b], Stroke::new(1.5, color));
    }
    focus_ring(ui, &response, (size / 2.0) as u8);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("common.close")));
    response
        .on_hover_text(t!("common.close_esc"))
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// A dialog over the window: a heading with a ×, then `add`. Returns what `add`
/// returned and whether it should close (×, Escape or a click outside).
pub fn dialog<R>(
    ctx: &Context,
    id: &str,
    title: &str,
    add: impl FnOnce(&mut Ui) -> R,
) -> (R, bool) {
    let mut close = false;
    let modal = Modal::new(Id::unique(("dialog", id)))
        .frame(dialog_frame(ctx))
        .show(ctx, |ui| {
            ui.set_width(DIALOG_WIDTH);
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(heading(title)).wrap());
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    close = close_button(ui, 28.0).clicked();
                });
            });
            ui.add_space(2.0);
            add(ui)
        });
    let close = close || modal.should_close();
    (modal.inner, close)
}

/// The frame of dialogs and the settings windows: the page surface, rounded, with a shadow.
pub fn dialog_frame(ctx: &Context) -> Frame {
    let style = ctx.global_style();
    let c = super::appearance(ctx).palette(style.visuals.dark_mode);
    Frame::new()
        .fill(c.main)
        .stroke(Stroke::new(1.0, c.border))
        .corner_radius(RADIUS_WINDOW)
        .inner_margin(Margin::same(DIALOG_PADDING))
        .shadow(style.visuals.window_shadow)
}

/// A card floating at the top right of the window, above everything but dialogs,
/// for a short-lived alert like a poke.
pub fn toast<R>(ctx: &Context, id: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    Area::new(Id::unique(("toast", id)))
        .order(Order::Foreground)
        .anchor(Align2::RIGHT_TOP, Vec2::new(-SPACE_L, SPACE_L))
        .show(ctx, |ui| {
            dialog_frame(ctx)
                .inner_margin(Margin::same(SPACE_M as i8))
                .show(ui, |ui| {
                    ui.set_width(POPUP_WIDTH);
                    add(ui)
                })
                .inner
        })
        .inner
}

/// The buttons at the bottom of a dialog, right-aligned: add the main one first.
pub fn footer(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    ui.add_space(SPACE_S);
    let size = Vec2::new(ui.available_width(), CONTROL_HEIGHT);
    ui.allocate_ui_with_layout(size, Layout::right_to_left(Align::Center), add);
}

/// A pick-one card with an icon, a title and a line of help, like the input mode cards.
pub fn choice_card(ui: &mut Ui, selected: bool, icon: &str, title: &str, help: &str) -> Response {
    let width = ((ui.available_width() - SPACE_S) / 2.0).clamp(150.0, 240.0);
    choice_card_sized(ui, width, selected, icon, title, help)
}

/// `choice_card` at a given width, for rows of more than two.
pub fn choice_card_sized(
    ui: &mut Ui,
    width: f32,
    selected: bool,
    icon: &str,
    title: &str,
    help: &str,
) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 64.0), Sense::click());
    let c = colors(ui);
    let a = accent(ui);
    let fill = if response.hovered() || selected {
        c.hover
    } else {
        ui.visuals().extreme_bg_color
    };
    let painter = ui.painter();
    painter.rect_filled(rect, RADIUS, fill);
    if selected {
        painter.rect_stroke(rect, RADIUS, Stroke::new(2.0, a.base), StrokeKind::Inside);
    } else {
        focus_ring(ui, &response, RADIUS);
    }
    let icon_color = if selected { a.text(c.dark) } else { c.muted };
    let mid = rect.center().y;
    painter.text(
        rect.left_center() + Vec2::new(24.0, 0.0),
        Align2::CENTER_CENTER,
        icon,
        FontId::proportional(22.0),
        icon_color,
    );
    let x = rect.left() + 46.0;
    painter.text(
        egui::pos2(x, mid - 1.0),
        Align2::LEFT_BOTTOM,
        title,
        FontId::new(TEXT_BODY, bold_family()),
        c.strong,
    );
    painter.text(
        egui::pos2(x, mid + 2.0),
        Align2::LEFT_TOP,
        help,
        FontId::proportional(TEXT_SMALL),
        c.muted,
    );
    response.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, selected, title));
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// A pick-one card with a small preview of a theme: its rail, sidebar, page, a few
/// lines of text and the accent.
pub fn theme_card(ui: &mut Ui, theme: &Palette, accent: &Accent, selected: bool) -> Response {
    let size = Vec2::new(150.0, 112.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let c = colors(ui);
    let painter = ui.painter();
    let preview = Rect::from_min_size(rect.min, Vec2::new(size.x, 80.0));
    let round = |nw, ne, sw, se| CornerRadius { nw, ne, sw, se };
    painter.rect_filled(preview, RADIUS, theme.main);
    let rail = Rect::from_min_size(preview.min, Vec2::new(18.0, preview.height()));
    painter.rect_filled(rail, round(RADIUS, 0, RADIUS, 0), theme.rail);
    let side = Rect::from_min_size(rail.right_top(), Vec2::new(40.0, preview.height()));
    painter.rect_filled(side, 0, theme.side);
    painter.circle_filled(rail.center_top() + Vec2::new(0.0, 12.0), 5.0, accent.base);
    painter.circle_filled(rail.center_top() + Vec2::new(0.0, 26.0), 5.0, theme.main);
    let selected_row = Rect::from_min_size(side.min + Vec2::new(4.0, 10.0), Vec2::new(32.0, 8.0));
    painter.rect_filled(selected_row, 2, theme.selected);
    for at in 0..3 {
        let row = Rect::from_min_size(
            side.min + Vec2::new(6.0, 24.0 + 10.0 * at as f32),
            Vec2::new(24.0, 4.0),
        );
        painter.rect_filled(row, 2, theme.muted);
    }
    let body = side.right_top() + Vec2::new(8.0, 10.0);
    for (at, width) in [52.0, 70.0, 40.0].into_iter().enumerate() {
        let y = 14.0 * at as f32;
        painter.circle_filled(body + Vec2::new(4.0, y + 3.0), 4.0, theme.muted);
        let line =
            Rect::from_min_size(body + Vec2::new(12.0, y + 1.0), Vec2::new(width * 0.6, 4.0));
        painter.rect_filled(line, 2, theme.text);
    }
    let button = Rect::from_min_size(
        preview.right_bottom() - Vec2::new(36.0, 16.0),
        Vec2::new(28.0, 9.0),
    );
    painter.rect_filled(button, 3, accent.base);
    let border = if selected {
        Stroke::new(2.0, accent.base)
    } else if response.hovered() {
        Stroke::new(1.0, c.muted)
    } else {
        Stroke::new(1.0, c.border)
    };
    painter.rect_stroke(preview, RADIUS, border, StrokeKind::Outside);
    focus_ring(ui, &response, RADIUS);
    let label = if selected {
        bold(theme.label()).color(c.strong)
    } else {
        RichText::new(theme.label()).color(c.text)
    };
    let label: WidgetText = label.into();
    let galley = label.into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        size.x,
        egui::TextStyle::Body,
    );
    let at = egui::pos2(rect.left() + 2.0, preview.bottom() + SPACE_S);
    ui.painter().galley(at, galley, c.text);
    response.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, selected, theme.label()));
    response.on_hover_cursor(CursorIcon::PointingHand)
}

/// A round accent swatch; the picked one has a ring and a check.
pub fn swatch(ui: &mut Ui, accent: &Accent, selected: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::click());
    let c = colors(ui);
    let painter = ui.painter();
    let center = rect.center();
    let fill = if response.hovered() {
        accent.strong
    } else {
        accent.base
    };
    painter.circle_filled(center, 14.0, fill);
    if selected {
        painter.circle_stroke(center, 17.0, Stroke::new(2.0, c.strong));
        painter.text(
            center,
            Align2::CENTER_CENTER,
            "✔",
            FontId::proportional(TEXT_ICON),
            ON_FILL,
        );
    }
    focus_ring(ui, &response, 18);
    response
        .widget_info(|| WidgetInfo::selected(Role::RadioButton, true, selected, accent.label()));
    response
        .on_hover_text(accent.label())
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// A round colour choice; `None` is "no colour", drawn as a struck ring. `label` names
/// it for screen readers and the tooltip.
pub fn color_swatch(ui: &mut Ui, color: Option<Color32>, selected: bool, label: &str) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
    let c = colors(ui);
    let painter = ui.painter();
    let center = rect.center();
    let grow = if response.hovered() { 1.0 } else { 0.0 };
    match color {
        Some(fill) => {
            painter.circle_filled(center, 11.0 + grow, fill);
        }
        None => {
            painter.circle_stroke(center, 10.0 + grow, Stroke::new(2.0, c.muted));
            let d = Vec2::splat(7.0);
            painter.line_segment([center - d, center + d], Stroke::new(2.0, c.muted));
        }
    }
    if selected {
        painter.circle_stroke(center, 14.0, Stroke::new(2.0, c.strong));
    }
    focus_ring(ui, &response, 15);
    response.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, selected, label));
    response
        .on_hover_text(label)
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// A small filled circle in a role's colour, or the muted text colour without one.
pub fn color_dot(ui: &mut Ui, color: Option<Color32>) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    let fill = color.unwrap_or(colors(ui).muted);
    ui.painter().circle_filled(rect.center(), 5.0, fill);
}

/// A small filled tag, like "LIVE" or "Open joining".
pub fn badge(ui: &mut Ui, text: &str, fill: Color32) -> Response {
    let text = RichText::new(text)
        .size(TEXT_CAPTION - 1.0)
        .family(bold_family())
        .color(ON_FILL);
    Frame::new()
        .fill(fill)
        .corner_radius(RADIUS_S)
        .inner_margin(Margin::symmetric(5, 1))
        .show(ui, |ui| ui.add(egui::Label::new(text).selectable(false)))
        .inner
}

/// The unread mark: a short pill `gap` left of a row or server button.
pub fn unread_pill(ui: &Ui, row: Rect, gap: f32) {
    let pill = Rect::from_center_size(
        egui::pos2(row.left() - gap, row.center().y),
        Vec2::new(4.0, 8.0),
    );
    ui.painter().rect_filled(pill, 2.0, colors(ui).strong);
}

/// A red count of mentions centred on `at`, ringed in `ring`, the surface it sits on.
pub fn mention_badge(ui: &Ui, at: Pos2, count: u32, ring: Color32) {
    let text = if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    };
    let painter = ui.painter();
    let galley = painter.layout_no_wrap(
        text,
        FontId::new(TEXT_CAPTION - 1.0, bold_family()),
        ON_FILL,
    );
    let size = Vec2::new((galley.size().x + 8.0).max(16.0), 16.0);
    let rect = Rect::from_center_size(at, size);
    let fill = colors(ui).danger;
    painter.rect(rect, 8.0, fill, Stroke::new(2.0, ring), StrokeKind::Outside);
    painter.galley(rect.center() - galley.size() / 2.0, galley, ON_FILL);
}

/// A floating copy of a row that follows the pointer, like a dragged channel.
pub fn drag_preview(ctx: &Context, id: Id, at: Pos2, width: f32, label: &str) {
    Area::new(id)
        .order(Order::Tooltip)
        .fixed_pos(at)
        .interactable(false)
        .show(ctx, |ui| {
            Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_width(width);
                ui.label(label);
            });
        });
}

/// A small × button, for removing a staged file or dismissing a line.
pub fn small_close(ui: &mut Ui, tip: &str) -> Response {
    icon_button(ui, "×", tip, Look::Ghost, Vec2::splat(22.0))
}

/// Text styling for an inline mention in chat.
pub fn mention_format(ui: &Ui) -> egui::text::TextFormat {
    let (color, background) = accent(ui).mention_colors(colors(ui));
    egui::text::TextFormat {
        font_id: FontId::new(TEXT_BODY, crate::emoji::family(true)),
        color,
        background,
        ..Default::default()
    }
}

/// Tint a message addressed to the reader and mark its left edge.
pub fn mention_highlight(ui: &Ui, background: egui::layers::ShapeIdx, rect: Rect, hovered: bool) {
    let accent = accent(ui);
    let palette = colors(ui);
    let surface = if hovered {
        palette.hover_soft
    } else {
        palette.main
    };
    ui.painter().set(
        background,
        Shape::rect_filled(rect, RADIUS_S, surface.lerp_to_gamma(accent.base, 0.09)),
    );
    let accent_bar = Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height()));
    ui.painter()
        .rect_filled(accent_bar, RADIUS_S, accent.text(ui.visuals().dark_mode));
}
