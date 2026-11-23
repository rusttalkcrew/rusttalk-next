//! The one menu used everywhere: roomy rows with an optional leading icon or dot, a
//! label and an optional line of help, and a trailing checkbox, tick or submenu
//! chevron. Danger items are red, disabled ones dimmed. Built on egui's popups and
//! submenus, so arrow keys, Enter, Escape and clicking outside behave as usual.

use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Galley, Popup, PopupCloseBehavior, Rect, RectAlign,
    Response, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo,
    accesskit::Role,
    containers::menu::SubMenu,
    text::{LayoutJob, TextWrapping},
};

use super::{
    HOVER_SECONDS, MENU_PADDING, MENU_ROW, MENU_ROW_TALL, MENU_WIDE, MENU_WIDTH, ON_FILL,
    RADIUS_ROW, RADIUS_S, TEXT_BODY, TEXT_CAPTION, TEXT_ICON, accent, bold_family, colors,
    focus_ring,
};

/// Indent of the leading icon's centre and of the label.
const ICON_X: f32 = 18.0;
const LABEL_X: f32 = 38.0;
const TRAILING_X: f32 = 18.0;
const CHECK_BOX: f32 = 16.0;

/// The inside of a menu; add items to it.
pub struct Menu<'u> {
    ui: &'u mut Ui,
    /// Set by `keep_open`.
    controls: bool,
}

enum Leading<'a> {
    Glyph(&'a str),
    Dot(Color32),
}

enum Trailing {
    None,
    Check(bool),
    /// A tick on the picked one of a few choices.
    Pick(bool),
    Chevron,
}

/// One menu row, built up and then added with [`Menu::add`].
pub struct Item<'a> {
    leading: Leading<'a>,
    label: &'a str,
    hint: Option<&'a str>,
    trailing: Trailing,
    danger: bool,
    enabled: bool,
}

impl<'a> Item<'a> {
    pub fn new(icon: &'a str, label: &'a str) -> Self {
        Self {
            leading: Leading::Glyph(icon),
            label,
            hint: None,
            trailing: Trailing::None,
            danger: false,
            enabled: true,
        }
    }

    /// A row led by a coloured dot instead of an icon, like a status.
    pub fn dot(color: Color32, label: &'a str) -> Self {
        Self {
            leading: Leading::Dot(color),
            ..Self::new("", label)
        }
    }

    /// A second, smaller line under the label.
    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = Some(hint);
        self
    }

    /// A checkbox on the right.
    pub fn checked(mut self, on: bool) -> Self {
        self.trailing = Trailing::Check(on);
        self
    }

    /// One of a few choices: a tick on the right when it is the picked one.
    pub fn picked(mut self, on: bool) -> Self {
        self.trailing = Trailing::Pick(on);
        self
    }

    /// Red, also on hover: for leaving, deleting, kicking.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// Dimmed and not clickable when `false`.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

impl Menu<'_> {
    /// Add a row; returns its response, clicked when picked.
    pub fn add(&mut self, item: Item) -> Response {
        let ui = &mut *self.ui;
        let response = ui.add_enabled_ui(item.enabled, |ui| row(ui, &item)).inner;
        // Also when `keep_open` lets other clicks through.
        if response.clicked() {
            ui.close();
        }
        response
    }

    /// Stay open while the controls added through `ui`, like sliders and mute
    /// buttons, are clicked; items still close the menu.
    pub fn keep_open(&mut self) {
        self.controls = true;
    }

    pub fn item(&mut self, icon: &str, label: &str) -> Response {
        self.add(Item::new(icon, label))
    }

    pub fn danger(&mut self, icon: &str, label: &str) -> Response {
        self.add(Item::new(icon, label).danger())
    }

    /// A row with a chevron that opens `add` as a submenu to the side, on hover,
    /// click or the right arrow key.
    pub fn submenu<R>(
        &mut self,
        icon: &str,
        label: &str,
        add: impl FnOnce(&mut Menu) -> R,
    ) -> Option<R> {
        let item = Item {
            trailing: Trailing::Chevron,
            ..Item::new(icon, label)
        };
        let response = row(self.ui, &item);
        SubMenu::new()
            .show(self.ui, &response, |ui| {
                ui.set_width(content_width(MENU_WIDTH));
                add(&mut Menu {
                    ui,
                    controls: false,
                })
            })
            .map(|inner| inner.inner)
    }

    /// A thin line between groups of items.
    pub fn separator(&mut self) {
        let ui = &mut *self.ui;
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::hover());
        let y = rect.center().y;
        let x = rect.x_range().shrink(4.0);
        ui.painter()
            .hline(x, y, Stroke::new(1.0, colors(ui).border));
    }

    /// A small heading over a group of items.
    pub fn caption(&mut self, text: &str) {
        let ui = &mut *self.ui;
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            super::caption_label(ui, text);
        });
    }

    /// The menu's own `Ui`, for anything else, like a volume slider.
    pub fn ui(&mut self) -> &mut Ui {
        self.ui
    }
}

/// Content width for a menu `total` wide, frame included.
fn content_width(total: f32) -> f32 {
    total - 2.0 * f32::from(MENU_PADDING)
}

fn row(ui: &mut Ui, item: &Item) -> Response {
    let height = if item.hint.is_some() {
        MENU_ROW_TALL
    } else {
        MENU_ROW
    };
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let enabled = ui.is_enabled();
    let c = colors(ui);
    let hovered = enabled && (response.hovered() || response.has_focus());
    let hover = ui
        .ctx()
        .animate_bool_with_time(response.id.with("hover"), hovered, HOVER_SECONDS);
    let (text, hover_fill) = if item.danger {
        (c.danger_text, c.danger.gamma_multiply(0.18))
    } else {
        (c.text.lerp_to_gamma(c.strong, hover), c.hover)
    };
    let dim = if enabled { 1.0 } else { 0.45 };
    let text = text.gamma_multiply(dim);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        RADIUS_ROW,
        Color32::TRANSPARENT.lerp_to_gamma(hover_fill, hover),
    );
    let mid = rect.center().y;
    let left = rect.left();
    match item.leading {
        Leading::Glyph("") => {}
        Leading::Glyph(icon) => {
            let color = if item.danger {
                text
            } else {
                c.muted.lerp_to_gamma(c.strong, hover).gamma_multiply(dim)
            };
            painter.text(
                egui::pos2(left + ICON_X, mid),
                Align2::CENTER_CENTER,
                icon,
                FontId::proportional(TEXT_ICON),
                color,
            );
        }
        Leading::Dot(color) => {
            painter.circle_filled(
                egui::pos2(left + ICON_X, mid),
                5.0,
                color.gamma_multiply(dim),
            );
        }
    }
    let right_padding = match item.trailing {
        Trailing::None => 12.0,
        _ => 44.0,
    };
    let label_room = (width - LABEL_X - right_padding).max(0.0);
    let label = one_line(
        ui,
        item.label,
        FontId::proportional(TEXT_BODY),
        text,
        label_room,
    );
    match item.hint {
        None => {
            painter.galley(
                egui::pos2(left + LABEL_X, mid - label.size().y / 2.0),
                label,
                text,
            );
        }
        Some(hint) => {
            let hint = one_line(
                ui,
                hint,
                FontId::proportional(TEXT_CAPTION),
                c.muted.gamma_multiply(dim),
                label_room,
            );
            let top = mid - (label.size().y + hint.size().y) / 2.0;
            let label_height = label.size().y;
            painter.galley(egui::pos2(left + LABEL_X, top), label, text);
            painter.galley(
                egui::pos2(left + LABEL_X, top + label_height),
                hint,
                c.muted,
            );
        }
    }
    let right = egui::pos2(rect.right() - TRAILING_X, mid);
    let selected = match item.trailing {
        Trailing::None => None,
        Trailing::Pick(on) => {
            if on {
                painter.text(
                    right,
                    Align2::CENTER_CENTER,
                    "✔",
                    FontId::proportional(TEXT_ICON),
                    accent(ui).text(c.dark),
                );
            }
            return finish(ui, response, item, Some((Role::RadioButton, on)));
        }
        Trailing::Chevron => {
            painter.text(
                right,
                Align2::CENTER_CENTER,
                "›",
                FontId::new(TEXT_ICON + 2.0, bold_family()),
                text,
            );
            None
        }
        Trailing::Check(on) => {
            let a = accent(ui);
            let mark = Rect::from_center_size(right, Vec2::splat(CHECK_BOX));
            let (fill, border) = if on {
                (a.base, a.base)
            } else {
                (c.field, c.muted)
            };
            painter.rect(
                mark,
                RADIUS_S,
                fill.gamma_multiply(dim),
                Stroke::new(1.0, border.gamma_multiply(dim)),
                StrokeKind::Inside,
            );
            if on {
                painter.text(
                    mark.center(),
                    Align2::CENTER_CENTER,
                    "✔",
                    FontId::proportional(TEXT_CAPTION),
                    ON_FILL,
                );
            }
            Some((Role::CheckBox, on))
        }
    };
    finish(ui, response, item, selected)
}

fn finish(ui: &Ui, response: Response, item: &Item, selected: Option<(Role, bool)>) -> Response {
    let enabled = ui.is_enabled();
    focus_ring(ui, &response, RADIUS_ROW);
    response.widget_info(|| match selected {
        Some((role, on)) => WidgetInfo::selected(role, enabled, on, item.label),
        None => WidgetInfo::labeled(Role::Button, enabled, item.label),
    });
    if enabled {
        response.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        response
    }
}

/// `text` on one line, cut off with "…" when wider than `width`.
fn one_line(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = TextWrapping::truncate_at_width(width);
    ui.painter().layout_job(job)
}

fn show<R>(popup: Popup, width: f32, add: impl FnOnce(&mut Menu) -> R) -> Option<R> {
    // What the menu held last frame: a click lands after it was shown.
    let keep = popup.get_id().with("keep open");
    let ctx = popup.ctx().clone();
    let width = super::fit(&ctx, width);
    let popup = if ctx.data(|data| data.get_temp(keep)).unwrap_or(false) {
        popup.close_behavior(PopupCloseBehavior::CloseOnClickOutside)
    } else {
        popup
    };
    popup
        .width(width)
        .show(|ui| {
            ui.set_width(content_width(width));
            ui.spacing_mut().item_spacing.y = 2.0;
            let mut menu = Menu {
                ui,
                controls: false,
            };
            let inner = add(&mut menu);
            let controls = menu.controls;
            ctx.data_mut(|data| data.insert_temp(keep, controls));
            inner
        })
        .map(|inner| inner.inner)
}

/// A menu that opens under `anchor` when it is clicked.
pub fn menu<R>(anchor: &Response, add: impl FnOnce(&mut Menu) -> R) -> Option<R> {
    menu_with_id(anchor, Popup::default_response_id(anchor), add)
}

pub fn menu_with_id<R>(
    anchor: &Response,
    id: egui::Id,
    add: impl FnOnce(&mut Menu) -> R,
) -> Option<R> {
    show(Popup::menu(anchor).id(id).gap(4.0), MENU_WIDTH, add)
}

/// A wider `menu`, for rows with a line of help.
pub fn menu_wide<R>(anchor: &Response, add: impl FnOnce(&mut Menu) -> R) -> Option<R> {
    show(Popup::menu(anchor).gap(4.0), MENU_WIDE, add)
}

/// A menu that opens under `anchor` when clicked, exactly as wide (at least the menu width).
pub fn menu_below<R>(anchor: &Response, add: impl FnOnce(&mut Menu) -> R) -> Option<R> {
    let width = anchor.rect.width().max(MENU_WIDTH);
    show(Popup::menu(anchor).gap(4.0), width, add)
}

/// A wide menu above `anchor` that stays open while its checkboxes are toggled,
/// for picking several things, like apps to share.
pub fn menu_above<R>(anchor: &Response, add: impl FnOnce(&mut Menu) -> R) -> Option<R> {
    let popup = Popup::menu(anchor)
        .gap(4.0)
        .align(RectAlign::TOP_START)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside);
    show(popup, MENU_WIDE, add)
}

/// Open at the pointer on a right click or a stationary long press.
pub fn context_menu<R>(anchor: &Response, add: impl FnOnce(&mut Menu) -> R) -> Option<R> {
    context_menu_with_id(anchor, Popup::default_response_id(anchor), add)
}

pub fn context_menu_with_id<R>(
    anchor: &Response,
    id: egui::Id,
    add: impl FnOnce(&mut Menu) -> R,
) -> Option<R> {
    let ctx = &anchor.ctx;
    let mut popup = Popup::context_menu(anchor).id(id);
    // A selectable label or link can own the hold inside a row. Nested menus
    // (e.g. a saved file inside a message) get first choice as children draw first.
    if anchor.enabled()
        && ctx.interaction_snapshot(|s| s.long_touched.is_some())
        && anchor.container_contains_pointer()
    {
        let key = egui::Id::unique("handled context hold");
        let pass = ctx.cumulative_pass_nr();
        let first = ctx.data_mut(|data| {
            let first = data.get_temp::<u64>(key) != Some(pass);
            data.insert_temp(key, pass);
            first
        });
        popup = popup.open_memory(first.then_some(egui::SetOpenCommand::Bool(true)));
    }
    show(popup, MENU_WIDTH, add)
}

#[cfg(any(target_os = "android", target_os = "ios", test))]
/// A text field's menu on a phone: it opens at the finger on a long press, or
/// when `open` (a tap on the field's cursor), and a plain tap closes it.
pub fn touch_menu<R>(anchor: &Response, open: bool, add: impl FnOnce(&mut Menu) -> R) -> Option<R> {
    let ctx = &anchor.ctx;
    let at = anchor.id.with("touch menu at");
    let opening = open || anchor.secondary_clicked();
    if opening {
        // A lifted finger leaves no pointer position, so keep where it pressed.
        let pressed = ctx.input(|input| input.pointer.press_origin());
        let pos = pressed.unwrap_or(anchor.rect.left_bottom());
        ctx.data_mut(|data| data.insert_temp(at, pos));
    }
    let pos = ctx
        .data(|data| data.get_temp(at))
        .unwrap_or(anchor.rect.left_bottom());
    let set = if opening {
        Some(egui::SetOpenCommand::Bool(true))
    } else if anchor.clicked() {
        Some(egui::SetOpenCommand::Bool(false))
    } else {
        None
    };
    show(
        Popup::menu(anchor).open_memory(set).at_position(pos),
        MENU_WIDTH,
        add,
    )
}

/// Whether the menu of `anchor` (from `menu` or `menu_below`) is open.
pub fn is_open(anchor: &Response) -> bool {
    Popup::is_id_open(&anchor.ctx, Popup::default_response_id(anchor))
}
