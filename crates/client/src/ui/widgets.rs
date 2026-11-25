//! The shared widgets: buttons, fields, checkbox, icon button, cards, rows, dialogs,
//! badges and the appearance pickers. They read every colour from the active theme.

use eframe::egui::{
    self, Align, Align2, Area, Button, Color32, Context, CornerRadius, CursorIcon, FontId, Frame,
    Id, Layout, Margin, Modal, ModalResponse, Order, Popup, PopupCloseBehavior, PopupKind, Pos2,
    Rect, RectAlign, Response, RichText, Sense, Shape, Stroke, StrokeKind, TextBuffer, TextEdit,
    Ui, UiBuilder, Vec2, Widget, WidgetInfo, WidgetText, accesskit::Role, text::LayoutJob,
};

use super::{
    Accent, CARD_SECONDS, CARD_SLIDE, CONTROL_HEIGHT, DIALOG_PADDING, DIALOG_WIDTH, HOVER_GROW,
    HOVER_SECONDS, LARGE_CONTROL, ON_FILL, POPUP_WIDTH, PRESS_SHRINK, Palette, RADIUS, RADIUS_ROW,
    RADIUS_S, RADIUS_WINDOW, SPACE_L, SPACE_M, SPACE_S, SPACE_XL, STATE_SECONDS, TEXT_BODY,
    TEXT_CAPTION, TEXT_ICON, TEXT_SMALL, accent, bold, bold_family, colors, heading,
};

/// Latch one toolbar after a completed touch tap. A swipe never clicks.
/// Returns (visible, first tap) so a tile can reserve its first tap for the tools.
pub fn touch_tools(response: &Response) -> (bool, bool) {
    let ctx = &response.ctx;
    if !ctx.input(|i| i.has_touch_screen()) {
        return (false, false);
    }
    let key = Id::unique("touch toolbar");
    let (tap, mouse) = ctx.input(|input| {
        let tap = input.pointer.primary_clicked()
            && input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Touch {
                        phase: egui::TouchPhase::End,
                        ..
                    }
                )
            });
        (
            tap,
            !input.any_touches() && input.pointer.hover_pos().is_some() && !tap,
        )
    });
    let inside = response.container_contains_pointer();
    ctx.data_mut(|data| {
        let selected = data.get_temp::<Id>(key) == Some(response.id);
        if selected && (mouse || (tap && !inside)) {
            data.remove::<Id>(key);
            // Keep drawing on the release frame: a floating button may extend past the row.
            (tap, false)
        } else if tap && inside {
            data.insert_temp(key, response.id);
            (true, !selected)
        } else {
            (selected, false)
        }
    })
}

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
        // Growing space on both sides centres the text in a button stretched wide.
        let text = RichText::new(text).family(bold_family()).size(text_size);
        ui.add(
            Button::new((egui::Atom::grow(), text, egui::Atom::grow()))
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

/// A reaction under a message: an emoji and its count. One of mine gets a faint accent
/// tint and an accent outline rather than a solid fill, so every emoji keeps its colour.
pub fn reaction_chip<'a>(ui: &mut Ui, mine: bool, label: impl egui::IntoAtoms<'a>) -> Response {
    let c = colors(ui);
    let (fill, stroke) = if mine {
        let accent = accent(ui).base;
        (accent.gamma_multiply(0.18), Stroke::new(1.0, accent))
    } else {
        (c.button.gamma_multiply(0.45), Stroke::new(1.0, c.border))
    };
    ui.add(
        Button::new(label)
            .fill(fill)
            .stroke(stroke)
            .corner_radius(RADIUS),
    )
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
    private: bool,
    password: bool,
}

/// A one-line text field, as tall as a button (a touch target on phones).
pub fn field(text: &mut dyn TextBuffer) -> Field<'_> {
    Field {
        edit: TextEdit::singleline(text)
            .margin(Margin::symmetric(10, 7))
            .min_size(Vec2::new(0.0, CONTROL_HEIGHT))
            .align(Align2::LEFT_CENTER),
        private: false,
        password: false,
    }
}

/// A text field of several lines.
pub fn text_area(text: &mut dyn TextBuffer) -> Field<'_> {
    Field {
        edit: TextEdit::multiline(text).margin(Margin::symmetric(10, 7)),
        private: false,
        password: false,
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

    /// Where egui keeps the field's cursor and undo history.
    pub fn id(mut self, id: egui::Id) -> Self {
        self.edit = self.edit.id(id);
        self
    }

    /// Secret but shown, like recovery words: phone keyboards offer no suggestions
    /// and don't learn it. Password fields get that already.
    pub fn private(mut self) -> Self {
        self.private = true;
        self
    }

    /// Show dots instead of the typed text.
    pub fn password(mut self) -> Self {
        self.password = true;
        self.edit = self.edit.password(true);
        self
    }

    pub fn char_limit(mut self, limit: usize) -> Self {
        self.edit = self.edit.char_limit(limit);
        self
    }

    /// A font that draws emoji too, like `crate::emoji::font`, for text people send.
    pub fn font(mut self, font: FontId) -> Self {
        self.edit = self.edit.font(font);
        self
    }
}

impl Widget for Field<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        if self.private {
            keep_private(ui.ctx());
        }
        let mut output = self.edit.show(ui);
        if self.private || self.password {
            // egui's undo history keeps copies of the text, which Ctrl+Z would
            // bring back after the field was emptied or its screen left.
            output.state.clear_undoer();
            output
                .state
                .clone()
                .store(ui.ctx(), output.response.response.id);
        }
        share_with_keyboard(&output, self.private || self.password);
        let response = output.response.response;
        if self.private {
            // For this frame only: another screen's field can get the same id.
            let pass = ui.ctx().cumulative_pass_nr();
            ui.ctx()
                .data_mut(|data| data.insert_temp(private_id(response.id), pass));
        }
        field_focus(ui, &response);
        response
    }
}

/// Phones: the on-screen keyboard works on the text around the cursor (to delete
/// and correct it), so the focused field shares it (`android::share_field`); a
/// `secret` field shares only its cursor and length. Phones also get a long-press
/// menu to paste, copy and cut (`phone_clipboard`).
pub fn share_with_keyboard(output: &egui::text_edit::TextEditOutput, secret: bool) {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    crate::phone_clipboard::menu(output, secret);
    #[cfg(any(target_os = "android", test))]
    if output.response.response.has_focus() {
        let response = &output.response.response;
        crate::android::share_field(
            &response.ctx,
            response.id,
            output.galley.text(),
            output.cursor_range,
            secret,
        );
    }
    #[cfg(not(any(target_os = "android", test)))]
    let _ = (output, secret);
}

fn private_id(field: egui::Id) -> egui::Id {
    field.with("private field")
}

/// Never copy or cut out of a focused private field (egui only protects password
/// fields): its text would reach the system clipboard, also from a phone.
fn keep_private(ctx: &egui::Context) {
    let Some(focused) = ctx.memory(|memory| memory.focused()) else {
        return;
    };
    // Marked last frame (the id is only known after showing the field).
    let marked = ctx.data(|data| data.get_temp::<u64>(private_id(focused)));
    if marked.is_some_and(|pass| pass + 1 >= ctx.cumulative_pass_nr()) {
        ctx.input_mut(|input| {
            input
                .events
                .retain(|event| !matches!(event, egui::Event::Copy | egui::Event::Cut));
        });
    }
}

/// Whether the field was marked [`Field::private`].
#[cfg(target_os = "android")]
pub fn is_private(ctx: &egui::Context, field: egui::Id) -> bool {
    ctx.data(|data| data.get_temp(private_id(field))) == Some(ctx.cumulative_pass_nr())
}

/// Give `field` focus while nothing has it, so typing can start at once. Not on
/// phones: focus there opens the keyboard, and taking it back at once would keep
/// a tap outside the field from ever closing the keyboard.
pub fn focus_when_none(ui: &Ui, field: &Response) {
    if !cfg!(any(target_os = "android", target_os = "ios"))
        && ui.memory(|memory| memory.focused().is_none())
    {
        field.request_focus();
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
        .width(fit(&anchor.ctx, POPUP_WIDTH))
        .show(|ui| {
            ui.set_width(fit(ui.ctx(), POPUP_WIDTH));
            add(ui)
        })
        .map(|inner| inner.inner)
}

/// A small card beside `anchor` that opens on a left click and closes on a click
/// outside or Escape, like someone's profile card. It has its own id, so `anchor`
/// can have a `context_menu` as well. With `at_click` it opens where `anchor` was
/// clicked instead, so on a big tile it covers that tile rather than its neighbour.
pub fn card_popup<R>(
    ui: &Ui,
    id: Id,
    anchor: &Response,
    label: &str,
    at_click: bool,
    add: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let id = id.with("card");
    let spot = id.with("spot");
    if at_click
        && anchor.clicked()
        && let Some(pos) = anchor.interact_pointer_pos()
    {
        ui.data_mut(|data| data.insert_temp(spot, pos));
    }
    let clicked_at = at_click
        .then(|| ui.data(|data| data.get_temp::<Pos2>(spot)))
        .flatten();
    let c = colors(ui);
    let frame = Frame::new()
        .fill(c.raised)
        .stroke(Stroke::new(1.0, c.border))
        .corner_radius(RADIUS_WINDOW)
        .inner_margin(Margin::same(SPACE_L as i8))
        .shadow(ui.visuals().popup_shadow);
    let mut popup = Popup::from_toggle_button_response(anchor);
    if let Some(pos) = clicked_at {
        popup = popup.anchor(pos);
    }
    popup
        .id(id)
        .kind(PopupKind::Popup)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        // Beside the row or tile; egui flips it when there is no room.
        .align(RectAlign::RIGHT_START)
        .gap(SPACE_S)
        .frame(frame)
        .accessible_name(label)
        .show(|ui| {
            ui.set_width(fit(ui.ctx(), POPUP_WIDTH) - 2.0 * SPACE_L);
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
    status_dot(ui, color, false);
}

/// A status dot; `barred` adds the bar across it that marks Do not disturb.
pub fn status_dot(ui: &mut Ui, color: Color32, barred: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    paint_dot(ui.painter(), rect.center(), 5.0, color, barred);
}

/// A filled dot, with the Do not disturb bar across it when `barred`.
pub fn paint_dot(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32, barred: bool) {
    painter.circle_filled(center, radius, color);
    if barred {
        dnd_bar(painter, center, radius);
    }
}

/// The light bar across a Do not disturb dot, like Discord's.
fn dnd_bar(painter: &egui::Painter, center: Pos2, radius: f32) {
    let bar = Vec2::new(radius * 1.2, (radius * 0.4).max(1.5));
    painter.rect_filled(Rect::from_center_size(center, bar), bar.y / 2.0, ON_FILL);
}

/// Corner radius of a rounded-square avatar or server icon of `size` points.
pub fn avatar_radius(size: f32) -> f32 {
    size.max(0.0) * 0.25
}

/// The green rounded-square outline around a speaking person's avatar.
/// Reaches 3 points past `rect` (1-point gap, 2-point stroke) with concentric corners.
pub fn speaking_outline(ui: &Ui, rect: Rect) {
    let offset = 2.0;
    let radius = avatar_radius(rect.width()) + offset;
    ui.painter().rect_stroke(
        rect.expand(offset),
        radius,
        Stroke::new(2.0, colors(ui).success),
        StrokeKind::Middle,
    );
}

/// Frames per second of the sound bars while they move.
const BARS_TICK: std::time::Duration = std::time::Duration::from_millis(66);

/// Three small bars that move with a sound's `level` (0 to 1) and lie still at
/// `None`. Only moving bars ask for another frame, so silence costs no redraws.
pub fn sound_bars(ui: &mut Ui, level: Option<f32>) -> Response {
    let size = Vec2::splat(TEXT_SMALL);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let c = colors(ui);
    let color = if level.is_some() { c.success } else { c.muted };
    let width = 3.0;
    let gap = (size.x - 3.0 * width) / 2.0;
    let heights = bar_heights(level, ui.input(|input| input.time));
    for (i, height) in heights.into_iter().enumerate() {
        let left = rect.left() + i as f32 * (width + gap);
        let bar = Rect::from_min_max(
            egui::pos2(left, rect.bottom() - height * size.y),
            egui::pos2(left + width, rect.bottom()),
        );
        ui.painter().rect_filled(bar, 1.0, color);
    }
    if level.is_some() {
        ui.ctx().request_repaint_after(BARS_TICK);
    }
    response
}

/// Each bar's height from 0 to 1 at `time` seconds: low and level in silence, else
/// the sound's level with its own wobble per bar.
fn bar_heights(level: Option<f32>, time: f64) -> [f32; 3] {
    const LOW: f32 = 0.25;
    let Some(level) = level else {
        return [LOW; 3];
    };
    std::array::from_fn(|i| {
        let i = i as f64;
        let wobble = (time * (6.0 + 2.5 * i) + 1.7 * i).sin().abs() as f32;
        LOW + (1.0 - LOW) * level.clamp(0.0, 1.0) * (0.35 + 0.65 * wobble)
    })
}

/// A presence dot on the bottom right of a picture, ringed in `ring` so it stands out;
/// `barred` marks Do not disturb.
pub fn presence_dot(
    ui: &Ui,
    picture: Rect,
    radius: f32,
    color: Color32,
    ring: Color32,
    barred: bool,
) {
    let center = picture.right_bottom() - Vec2::splat(radius * 0.8);
    ui.painter().circle(center, radius, color, (2.0, ring));
    if barred {
        dnd_bar(ui.painter(), center, radius);
    }
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

/// A row with `trailing` (buttons, added right to left) at its right end and
/// `content` in the room left of them, top down, so long text wraps or is cut short
/// instead of running under the buttons on a narrow window.
pub fn trailing_row<R>(
    ui: &mut Ui,
    trailing: impl FnOnce(&mut Ui),
    content: impl FnOnce(&mut Ui) -> R,
) -> R {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            trailing(ui);
            ui.with_layout(Layout::top_down(Align::Min), content).inner
        })
        .inner
    })
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
    nav_row_trailing(ui, icon, label, selected, role, accessible, 0.0)
}

/// `nav_row` that keeps `trailing` points at its right edge free for a button; a
/// longer label is cut short with an ellipsis instead.
pub fn nav_row_trailing(
    ui: &mut Ui,
    icon: &str,
    label: &str,
    selected: bool,
    role: Role,
    accessible: &str,
    trailing: f32,
) -> Response {
    nav_row_locked(ui, icon, label, selected, role, accessible, trailing, false)
}

/// Room `nav_row_locked` keeps at the right for its lock.
pub const NAV_LOCK_WIDTH: f32 = 24.0;

/// `nav_row_trailing` with a lock at the right end when `locked`, for a channel
/// only some roles can see.
#[allow(clippy::too_many_arguments)]
pub fn nav_row_locked(
    ui: &mut Ui,
    icon: &str,
    label: &str,
    selected: bool,
    role: Role,
    accessible: &str,
    trailing: f32,
    locked: bool,
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
    let mut job = LayoutJob::simple(
        label.to_owned(),
        label_font,
        text,
        (rect.width() - 44.0 - trailing - if locked { NAV_LOCK_WIDTH } else { 0.0 }).max(0.0),
    );
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = painter.layout_job(job);
    let at = egui::pos2(rect.left() + 34.0, rect.center().y - galley.size().y / 2.0);
    painter.with_clip_rect(rect).galley(at, galley, text);
    if locked {
        // At the very end, moving over only while the button in `trailing` shows (hovered
        // or open), so locks line up down the list.
        let shift = if selected || ui.rect_contains_pointer(rect) {
            trailing
        } else {
            0.0
        };
        lock_icon(
            painter,
            egui::pos2(rect.right() - shift - 13.0, rect.center().y),
            text,
        );
    }
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
    // Touch screens have no Escape key to mention.
    let tip = if super::TOUCH {
        t!("common.close")
    } else {
        t!("common.close_esc")
    };
    response
        .on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// A dialog over the window: a heading with a ×, then `add`. Returns what `add`
/// returned and whether it should close (×, Escape or a click outside). Phones, and
/// touch screens too short for a centred dialog (a phone held sideways), show it as
/// a full-screen sheet with a top bar; elsewhere it is centred and scrolls when the
/// window is too short for it.
pub fn dialog<R>(
    ctx: &Context,
    id: &str,
    title: &str,
    add: impl FnOnce(&mut Ui) -> R,
) -> (R, bool) {
    touch_dialog(ctx, id, title, super::TOUCH, add)
}

/// `dialog`, as on a touch screen or not.
fn touch_dialog<R>(
    ctx: &Context,
    id: &str,
    title: &str,
    touch: bool,
    add: impl FnOnce(&mut Ui) -> R,
) -> (R, bool) {
    let id = Id::unique(("dialog", id));
    if dialog_as_sheet(ctx, touch) {
        let modal = sheet(ctx, id, |ui| {
            let (_, close) = sheet_bar(ui, title, None);
            let inner = sheet_page(ui, id, add);
            (inner, close)
        });
        let close = modal.should_close();
        let (inner, closed) = modal.inner;
        return (inner, closed || close);
    }
    let mut close = false;
    // Leave the window's edge visible around it.
    let margin = 2.0 * (SPACE_L + f32::from(DIALOG_PADDING));
    let room = ctx.content_rect().size() - Vec2::splat(margin);
    let modal = Modal::new(id).frame(dialog_frame(ctx)).show(ctx, |ui| {
        ui.set_width(DIALOG_WIDTH.min(room.x));
        // The area offers last frame's size; offer the whole window instead, so the
        // body below can grow when its content does (a tab switch, say).
        ui.set_max_height(room.y.max(SPACE_XL * 4.0));
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(heading(title)).wrap());
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                let size = if touch { super::TOUCH_TARGET } else { 28.0 };
                close = close_button(ui, size).clicked();
            });
        });
        ui.add_space(2.0);
        // `add` holds the buttons too, so the whole body scrolls when the window is
        // short; it shrinks to fit, so roomy windows look the same as without it.
        egui::ScrollArea::vertical()
            .id_salt(id.with("body"))
            .auto_shrink([false, true])
            .show(ui, add)
            .inner
    });
    let close = close || modal.should_close();
    (modal.inner, close)
}

/// Whether `dialog` shows as a full-screen sheet: on phones, and on touch screens
/// shorter than a phone is wide. The full window height counts, not the part above
/// the on-screen keyboard, so a dialog doesn't change shape while typing.
fn dialog_as_sheet(ctx: &Context, touch: bool) -> bool {
    super::is_phone(ctx) || (touch && ctx.viewport_rect().height() < super::PHONE_WIDTH)
}

/// A modal that covers the whole window, for dialogs and settings on phones.
pub fn sheet<R>(ctx: &Context, id: Id, add: impl FnOnce(&mut Ui) -> R) -> ModalResponse<R> {
    let screen = ctx.content_rect();
    let c = super::appearance(ctx).palette(ctx.global_style().visuals.dark_mode);
    Modal::new(id)
        .area(Modal::default_area(id).anchor(Align2::LEFT_TOP, Vec2::ZERO))
        .frame(Frame::new().fill(c.main))
        .show(ctx, |ui| {
            ui.set_min_size(screen.size());
            ui.set_max_size(screen.size());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        })
}

/// The height of `sheet_bar`.
pub const SHEET_BAR: f32 = super::TOUCH_TARGET + SPACE_S;

/// The top bar of a `sheet`: Back with `back` as its label (when given), the title
/// and a ×. Returns whether Back and × were tapped.
pub fn sheet_bar(ui: &mut Ui, title: &str, back: Option<&str>) -> (bool, bool) {
    let c = colors(ui);
    let mut pressed = (false, false);
    let size = Vec2::new(ui.available_width(), SHEET_BAR);
    let bar = ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_size(size);
        ui.add_space(SPACE_S);
        if let Some(back) = back {
            let button = Vec2::splat(super::TOUCH_TARGET);
            pressed.0 = icon_button(ui, "⬅", back, Look::Ghost, button).clicked();
        } else {
            ui.add_space(SPACE_S);
        }
        let close = super::TOUCH_TARGET + SPACE_S;
        ui.scope(|ui| {
            ui.set_max_width((ui.available_width() - close).max(0.0));
            ui.add(egui::Label::new(bold(title).size(super::TEXT_TITLE)).truncate());
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(SPACE_S);
            pressed.1 = close_button(ui, 32.0).clicked();
        });
    });
    let rect = bar.response.rect;
    ui.painter()
        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, c.border));
    pressed
}

/// The rest of a `sheet` under its bar: `add` in a scrolling page with a gutter.
pub fn sheet_page<R>(ui: &mut Ui, id: Id, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::ScrollArea::vertical()
        .id_salt(id.with("page"))
        .auto_shrink(false)
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin::same(SPACE_L as i8))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = SPACE_S;
                    ui.set_width(ui.available_width());
                    add(ui)
                })
                .inner
        })
        .inner
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
                    ui.set_width(fit(ctx, POPUP_WIDTH));
                    add(ui)
                })
                .inner
        })
        .inner
}

/// `width`, or less on a window too narrow to show it with a gutter on both sides:
/// for things that float, like popups, menus and toasts.
pub fn fit(ctx: &Context, width: f32) -> f32 {
    width.min(ctx.content_rect().width() - 2.0 * (SPACE_L + SPACE_M))
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
    // Cut short with "…" in a card narrowed by a small window.
    let line = |text: &str, font: FontId, color: Color32| {
        let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
        job.wrap = egui::text::TextWrapping::truncate_at_width(rect.right() - x - SPACE_S);
        painter.layout_job(job)
    };
    let heading = line(title, FontId::new(TEXT_BODY, bold_family()), c.strong);
    painter.galley(
        egui::pos2(x, mid - 1.0 - heading.size().y),
        heading,
        c.strong,
    );
    let help = line(help, FontId::proportional(TEXT_SMALL), c.muted);
    painter.galley(egui::pos2(x, mid + 2.0), help, c.muted);
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
    painter.rect_filled(
        Rect::from_center_size(rail.center_top() + Vec2::new(0.0, 12.0), Vec2::splat(10.0)),
        avatar_radius(10.0),
        accent.base,
    );
    painter.rect_filled(
        Rect::from_center_size(rail.center_top() + Vec2::new(0.0, 26.0), Vec2::splat(10.0)),
        avatar_radius(10.0),
        theme.main,
    );
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
        painter.rect_filled(
            Rect::from_center_size(body + Vec2::new(4.0, y + 3.0), Vec2::splat(8.0)),
            avatar_radius(8.0),
            theme.muted,
        );
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
    // A scroll area clips at the row's left edge; the pill sits just outside it.
    let mut clip = ui.clip_rect();
    clip.min.x = clip.min.x.min(pill.left());
    ui.painter()
        .with_clip_rect(clip)
        .rect_filled(pill, 2.0, colors(ui).strong);
}

/// A red count of mentions centred on `at`, ringed in `ring`, the surface it sits on.
pub fn mention_badge(ui: &Ui, at: Pos2, count: u32, ring: Color32) {
    let text = if count > 99 {
        "99+".to_owned()
    } else {
        count.to_string()
    };
    ringed_badge(ui, at, text, ring);
}

/// A painted availability badge, so the server button still owns the whole tap target.
pub fn unavailable_badge(ui: &Ui, at: Pos2, ring: Color32) {
    ringed_badge(ui, at, "!".to_owned(), ring);
}

fn ringed_badge(ui: &Ui, at: Pos2, text: String, ring: Color32) {
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

/// A small filled padlock centred on `at`, drawn rather than taken from a font so it
/// stays crisp and takes the colour of the text next to it.
pub fn lock_icon(painter: &egui::Painter, at: Pos2, color: Color32) {
    let body = Rect::from_center_size(at + Vec2::new(0.0, 2.5), Vec2::new(10.0, 8.0));
    let radius = 3.0;
    // The shackle: a half ring standing on the body.
    let steps = 16;
    let mut points = vec![egui::pos2(at.x - radius, body.top())];
    points.extend((0..=steps).map(|step| {
        let angle = std::f32::consts::PI * (1.0 + step as f32 / steps as f32);
        egui::pos2(
            at.x + radius * angle.cos(),
            body.top() - 1.0 + radius * angle.sin(),
        )
    }));
    points.push(egui::pos2(at.x + radius, body.top()));
    painter.add(egui::epaint::PathShape::line(
        points,
        Stroke::new(1.6, color),
    ));
    painter.rect_filled(body, 2, color);
}

#[cfg(test)]
mod tests {
    use eframe::egui::{RawInput, SafeAreaInsets, Theme, epaint::MarginF32};
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    fn harness<S>(
        size: Vec2,
        dark: bool,
        state: S,
        mut show: impl FnMut(&Context, &mut S) + 'static,
    ) -> Harness<'static, S> {
        let theme = if dark { Theme::Dark } else { Theme::Light };
        let mut harness = Harness::builder()
            .with_size(size)
            .with_theme(theme)
            .with_context_setup(super::super::install)
            .build_ui_state(move |ui, state| show(ui.ctx(), state), state);
        harness.run_steps(3);
        harness
    }

    /// A dialog taller than a short window: many lines, then its button.
    fn tall(size: Vec2, dark: bool, touch: bool) -> Harness<'static, bool> {
        harness(size, dark, false, move |ctx, saved| {
            touch_dialog(ctx, "tall", "Tall", touch, |ui| {
                for line in 0..30 {
                    ui.label(format!("Line {line}"));
                }
                footer(ui, |ui| *saved |= primary(ui, "Save").clicked());
            });
        })
    }

    /// A small confirmation, like Sign out.
    fn short(size: Vec2, dark: bool, touch: bool) -> Harness<'static, ()> {
        harness(size, dark, (), move |ctx, ()| {
            touch_dialog(ctx, "short", "Sign out?", touch, |ui| {
                ui.label("You'll need your recovery words to sign in again.");
                footer(ui, |ui| {
                    danger(ui, "Sign out");
                    secondary(ui, "Cancel");
                });
            });
        })
    }

    #[test]
    fn short_touch_windows_get_the_sheet() {
        let ctx = Context::default();
        let sized = |width: f32, height: f32, keyboard: f32| {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, height))),
                safe_area_insets: Some(SafeAreaInsets(MarginF32 {
                    bottom: keyboard,
                    ..Default::default()
                })),
                ..Default::default()
            };
            ctx.run_ui(input, |_| {}).textures_delta.clear();
        };
        // A phone held sideways: a sheet on touch, a centred dialog with a mouse.
        sized(844.0, 390.0, 0.0);
        assert!(dialog_as_sheet(&ctx, true));
        assert!(!dialog_as_sheet(&ctx, false));
        // Phones upright always; tablets and desktops never, even with a keyboard up.
        sized(390.0, 844.0, 0.0);
        assert!(dialog_as_sheet(&ctx, false));
        sized(1180.0, 820.0, 400.0);
        assert!(!dialog_as_sheet(&ctx, true));
        sized(1100.0, 700.0, 0.0);
        assert!(!dialog_as_sheet(&ctx, false));
    }

    #[test]
    fn tall_dialogs_scroll_to_their_button_on_short_windows() {
        // A short desktop window, and a phone held sideways (a sheet there).
        for (width, height, touch) in [(1100.0, 500.0, false), (844.0, 390.0, true)] {
            let mut harness = tall(Vec2::new(width, height), true, touch);
            let title = harness.get_by_label("Tall").rect();
            assert!(title.top() >= 0.0, "the title stays on screen: {title:?}");
            let save = harness.get_by_label("Save").rect();
            assert!(save.bottom() > height, "the button starts below the fold");
            harness.get_by_label("Save").scroll_to_me();
            harness.run_steps(3);
            let save = harness.get_by_label("Save").rect();
            assert!(
                save.top() >= 0.0 && save.bottom() <= height,
                "scrolled into view: {save:?}"
            );
            harness.get_by_label("Save").click();
            harness.run_steps(2);
            assert!(*harness.state(), "the button is reachable");
        }
    }

    #[test]
    fn roomy_windows_show_the_whole_dialog() {
        let height = 700.0;
        let harness = short(Vec2::new(1100.0, height), true, false);
        let title = harness.get_by_label("Sign out?").rect();
        let button = harness.get_by_label("Cancel").rect();
        // Centred and only as tall as its content: nothing scrolls or is cut off.
        let middle = (title.top() + button.bottom()) / 2.0;
        assert!((middle - height / 2.0).abs() < 20.0, "centred: {middle}");
        assert!(button.bottom() - title.top() < 200.0, "no extra height");
        let close = harness.get_by_label(t!("common.close")).rect();
        assert_eq!(close.width(), 28.0);
        // A tablet held sideways keeps the centred dialog, with a full-size ×.
        let harness = short(Vec2::new(1180.0, 820.0), true, true);
        let close = harness.get_by_label(t!("common.close")).rect();
        assert_eq!(close.width(), super::super::TOUCH_TARGET);
    }

    #[test]
    fn dialogs_grow_with_their_content() {
        // (grown, saved): a tab switch, say, makes the body taller after it showed.
        let mut harness = harness(
            Vec2::new(1100.0, 700.0),
            true,
            (false, false),
            |ctx, (grown, saved)| {
                dialog(ctx, "grow", "Grow", |ui| {
                    let lines = if *grown { 12 } else { 1 };
                    for line in 0..lines {
                        ui.label(format!("Line {line}"));
                    }
                    footer(ui, |ui| *saved |= primary(ui, "Save").clicked());
                });
            },
        );
        harness.state_mut().0 = true;
        harness.run_steps(3);
        harness.get_by_label("Save").click();
        harness.run_steps(2);
        assert!(harness.state().1, "the grown dialog shows its button");
    }

    /// Set RUSTTALK_TEST_SCREENSHOTS to a folder to save how dialogs look.
    #[test]
    fn dialog_screenshots() {
        let Ok(dir) = std::env::var("RUSTTALK_TEST_SCREENSHOTS") else {
            return;
        };
        std::fs::create_dir_all(&dir).unwrap();
        let sizes = [
            (1100.0, 700.0, false),
            (1100.0, 500.0, false),
            (844.0, 390.0, true),
        ];
        for (width, height, touch) in sizes {
            for dark in [true, false] {
                let theme = if dark { "dark" } else { "light" };
                let size = Vec2::new(width, height);
                let mut short = short(size, dark, touch);
                let mut tall = tall(size, dark, touch);
                for (name, image) in [("short", short.render()), ("tall", tall.render())] {
                    let path = format!("{dir}/dialog-{name}-{width}x{height}-{theme}.png");
                    image.unwrap().save(path).unwrap();
                }
            }
        }
    }

    #[test]
    fn sound_bars_lie_still_in_silence_and_move_with_the_level() {
        let still = bar_heights(None, 0.0);
        assert_eq!(still, bar_heights(None, 1.3));
        let loud: Vec<_> = (0..20)
            .map(|i| bar_heights(Some(1.0), i as f64 * 0.07))
            .collect();
        assert!(loud.iter().flatten().all(|h| (still[0]..=1.0).contains(h)));
        assert!(loud.windows(2).any(|w| w[0] != w[1]), "they move over time");
        let quiet = bar_heights(Some(0.1), 0.5);
        let full = bar_heights(Some(1.0), 0.5);
        assert!(quiet.iter().zip(full).all(|(q, f)| *q <= f));
    }
}
