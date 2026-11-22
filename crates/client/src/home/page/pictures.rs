//! A page's banner and the pictures in its text, for reading and for editing in place.

use std::path::PathBuf;

use eframe::egui::{
    Align2, Color32, CornerRadius, CursorIcon, FontId, Id as UiId, Rect, Response, Role, Sense,
    Stroke, StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo, pos2, vec2,
};
use rusttalk_core::pages::{Align, Banner, MAX_BANNER, MIN_BANNER, MIN_PICTURE, Picture};
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

use crate::{
    files::{Files, Shown},
    net::Command,
    server::Id,
    ui,
};

/// A picture's height until it loaded, for its width.
const UNKNOWN_SHAPE: f32 = 9.0 / 16.0;
/// A picture taller than this many times its width is cut.
const TALLEST: f32 = 4.0;
/// A banner in a narrow window still shows something.
const LOWEST_BANNER: f32 = 72.0;
/// The strip that resizes a picture or the banner.
const GRAB: f32 = 12.0;
const BUTTON: f32 = 26.0;
/// The banner is the top of the page, so it has the page's corners.
const BANNER_CORNERS: CornerRadius = CornerRadius {
    nw: ui::RADIUS_WINDOW,
    ne: ui::RADIUS_WINDOW,
    sw: 0,
    se: 0,
};

/// Where pictures come from: this page's server, or this device until the page is saved.
pub(super) struct Art<'a> {
    pub files: &'a mut Files,
    pub address: &'a Url,
    pub commands: &'a UnboundedSender<Command>,
    /// Added in this edit, by the ID that stands in until they are uploaded.
    pub local: Vec<(Id, PathBuf)>,
}

impl Art<'_> {
    fn shown(&mut self, ui: &mut Ui, file: &rusttalk_core::client::Attachment) -> Shown {
        match self.local.iter().find(|(id, _)| *id == file.file.id) {
            Some((_, path)) => self.files.local_picture(ui, path),
            None => self.files.page_picture(ui, self.commands, file),
        }
    }

    fn fill(
        &mut self,
        ui: &mut Ui,
        rect: Rect,
        shown: &Shown,
        focus: [u8; 2],
        corners: CornerRadius,
    ) {
        let painter = ui.painter();
        let c = ui::colors(ui);
        match shown {
            Shown::Ready(texture) => {
                let uv = cover(texture.size_vec2(), rect.size(), focus);
                let mesh = eframe::egui::epaint::RectShape::filled(rect, corners, Color32::WHITE)
                    .with_texture(texture.id(), uv);
                painter.add(mesh);
            }
            Shown::Loading => {
                painter.rect_filled(rect, ui::RADIUS, c.field);
                let at = Rect::from_center_size(rect.center(), Vec2::splat(24.0));
                ui.put(at, eframe::egui::Spinner::new());
            }
            Shown::Gone | Shown::Failed => {
                painter.rect_filled(rect, ui::RADIUS, c.field);
                let text = match shown {
                    Shown::Gone => t!("home.picture_gone"),
                    _ => t!("home.picture_failed"),
                };
                let font = FontId::proportional(ui::TEXT_SMALL);
                painter.text(rect.center(), Align2::CENTER_CENTER, text, font, c.muted);
            }
        }
    }
}

/// The part of a picture of `size` that fills `room`, placed by `focus`.
fn cover(size: Vec2, room: Vec2, focus: [u8; 2]) -> Rect {
    let scale = (room.x / size.x.max(1.0)).max(room.y / size.y.max(1.0));
    let part = vec2(
        (room.x / (size.x.max(1.0) * scale).max(f32::EPSILON)).min(1.0),
        (room.y / (size.y.max(1.0) * scale).max(f32::EPSILON)).min(1.0),
    );
    let at = |axis: usize, part: f32| (1.0 - part) * f32::from(focus[axis]) / 255.0;
    Rect::from_min_size(pos2(at(0, part.x), at(1, part.y)), part)
}

/// How far the picture reaches past `room` each way, once it fills it.
fn overflow(size: Vec2, room: Vec2) -> Vec2 {
    let scale = (room.x / size.x.max(1.0)).max(room.y / size.y.max(1.0));
    (size * scale - room).max(Vec2::ZERO)
}

/// Where a picture goes in a line from `left`, `room` wide.
fn place(left: f32, room: f32, percent: u8, align: Align) -> (f32, f32) {
    let width = room * f32::from(percent) / 100.0;
    let space = room - width;
    let x = match align {
        Align::Left => left,
        Align::Center => left + space / 2.0,
        Align::Right => left + space,
    };
    (x, width)
}

/// The width in percent that puts the dragged edge under the pointer, in fives.
fn dragged_width(left: f32, room: f32, pointer: f32, align: Align) -> u8 {
    let width = match align {
        Align::Left => pointer - left,
        Align::Center => 2.0 * (pointer - (left + room / 2.0)),
        Align::Right => left + room - pointer,
    };
    let percent = ((width * 100.0 / room.max(1.0)) / 5.0).round() * 5.0;
    percent.clamp(f32::from(MIN_PICTURE), 100.0) as u8
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PictureChange {
    Width(u8),
    Align(Align),
    Up,
    Down,
    Remove,
}

fn lay_out(ui: &mut Ui, art: &mut Art, picture: &Picture, sense: Sense) -> (Rect, Response, Rect) {
    let shown = art.shown(ui, &picture.file);
    let shape = match &shown {
        Shown::Ready(texture) => {
            let size = texture.size_vec2();
            (size.y / size.x.max(1.0)).min(TALLEST)
        }
        _ => UNKNOWN_SHAPE,
    };
    ui.add_space(ui::SPACE_S);
    let room = ui.available_width();
    let left = ui.cursor().left();
    let (x, width) = place(left, room, picture.width, picture.align);
    let (line, response) = ui.allocate_exact_size(vec2(room, width * shape), sense);
    let rect = Rect::from_min_size(pos2(x, line.top()), vec2(width, width * shape));
    art.fill(ui, rect, &shown, [128, 128], CornerRadius::ZERO);
    ui.add_space(ui::SPACE_S);
    (rect, response, line)
}

/// A picture as readers see it; a click opens it large.
pub(super) fn picture(ui: &mut Ui, art: &mut Art, picture: &Picture, opens: bool) {
    let sense = if opens {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response, _) = lay_out(ui, art, picture, sense);
    if !opens {
        return;
    }
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.picture_open")));
    ui::focus_ring(ui, &response, ui::RADIUS);
    let over = response.hover_pos().is_some_and(|pos| rect.contains(pos));
    if over {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    if response.clicked() && (over || response.has_focus()) {
        art.files.view(art.address, art.commands, &picture.file);
    }
}

/// A picture in the tile being written: its buttons, and its edge to resize it.
pub(super) fn edit_picture(
    ui: &mut Ui,
    art: &mut Art,
    picture: &Picture,
    id: UiId,
) -> Option<PictureChange> {
    let (rect, _, line) = lay_out(ui, art, picture, Sense::hover());
    let mut change = None;
    let edge = match picture.align {
        Align::Right => rect.left(),
        Align::Left | Align::Center => rect.right(),
    };
    let grab = Rect::from_center_size(pos2(edge, rect.center().y), vec2(GRAB, rect.height()));
    let handle = ui.interact(grab, id.with("width"), Sense::drag());
    handle.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.picture_resize")));
    if handle.dragged()
        && let Some(pointer) = handle.interact_pointer_pos()
    {
        let wanted = dragged_width(line.left(), line.width(), pointer.x, picture.align);
        if wanted != picture.width {
            change = Some(PictureChange::Width(wanted));
        }
    }
    let accent = ui::accent(ui).base;
    let color = match handle.hovered() || handle.dragged() {
        true => accent,
        false => ui::colors(ui).border,
    };
    ui.painter()
        .rect_stroke(rect, 0, Stroke::new(1.0, color), StrokeKind::Inside);
    let bar = Rect::from_center_size(grab.center(), vec2(5.0, rect.height().min(36.0)));
    ui.painter().rect_filled(bar, 2.0, accent);
    handle
        .on_hover_text(t!("home.picture_resize"))
        .on_hover_cursor(CursorIcon::ResizeHorizontal);

    let sides = [
        (Align::Left, t!("home.picture_left")),
        (Align::Center, t!("home.picture_center")),
        (Align::Right, t!("home.picture_right")),
    ];
    let buttons = [
        ("⏶", t!("home.picture_up"), PictureChange::Up),
        ("⏷", t!("home.picture_down"), PictureChange::Down),
        ("🗑", t!("home.picture_remove"), PictureChange::Remove),
    ];
    // Under the picture, so the buttons fit however small it is.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (side, tip) in sides {
            let chosen = side == picture.align;
            let look = match chosen {
                true => ui::Look::On(accent),
                false => ui::Look::Ghost,
            };
            let button = ui::icon_button(ui, "", tip, look, Vec2::splat(BUTTON));
            let color = match chosen {
                true => ui::ON_FILL,
                false => ui::colors(ui).muted,
            };
            aligned_lines(ui, button.rect, side, color);
            if button.clicked() {
                change = Some(PictureChange::Align(side));
            }
        }
        ui.add_space(ui::SPACE_S);
        for (glyph, tip, does) in buttons {
            let look = ui::Look::Ghost;
            if ui::icon_button(ui, glyph, tip, look, Vec2::splat(BUTTON)).clicked() {
                change = Some(does);
            }
        }
    });
    change
}

/// The fonts' arrows read as moving, so alignment is drawn the way text editors do.
fn aligned_lines(ui: &Ui, button: Rect, side: Align, color: Color32) {
    const LONG: f32 = 14.0;
    const SHORT: f32 = 8.0;
    let left = button.center().x - LONG / 2.0;
    for (line, width) in [LONG, SHORT, LONG].into_iter().enumerate() {
        let (x, _) = place(left, LONG, (width * 100.0 / LONG).round() as u8, side);
        let y = button.center().y + (line as f32 - 1.0) * 4.0;
        ui.painter()
            .hline(x..=x + width, y, Stroke::new(1.5, color));
    }
}

/// From edge to edge of the page, which is wider than the room for its content.
fn banner_rect(ui: &Ui, banner: &Banner, widest: f32) -> Rect {
    let across = ui.clip_rect().x_range();
    let height = (across.span().min(widest) * f32::from(banner.height) / 1000.0).max(LOWEST_BANNER);
    Rect::from_x_y_ranges(across, ui.cursor().top()..=ui.cursor().top() + height)
}

/// Takes up the banner's height, but no more than the content's width.
fn after_banner(ui: &mut Ui, rect: Rect) {
    let content = rect.with_min_x(ui.max_rect().left());
    ui.advance_cursor_after_rect(content.with_max_x(ui.max_rect().right()));
}

/// The banner as readers see it. `widest` is the page's column, which sets its height.
pub(super) fn banner(ui: &mut Ui, art: &mut Art, banner: &Banner, widest: f32) {
    let rect = banner_rect(ui, banner, widest);
    let shown = art.shown(ui, &banner.file);
    art.fill(ui, rect, &shown, banner.focus, BANNER_CORNERS);
    after_banner(ui, rect);
}

#[derive(Clone, Copy)]
pub(super) enum BannerChange {
    Focus([u8; 2]),
    Height(u16),
    Replace,
    Remove,
}

/// The banner while editing: drag it to choose what shows, its lower edge for the height.
pub(super) fn edit_banner(
    ui: &mut Ui,
    art: &mut Art,
    banner: &Banner,
    widest: f32,
    channel: Id,
) -> Option<BannerChange> {
    let rect = banner_rect(ui, banner, widest);
    let shown = art.shown(ui, &banner.file);
    art.fill(ui, rect, &shown, banner.focus, BANNER_CORNERS);
    let mut change = None;

    let id = UiId::unique(("page-banner", channel));
    let body = ui.interact(rect, id, Sense::drag());
    body.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.banner_drag")));
    let start = id.with("start");
    if body.drag_started() {
        ui.data_mut(|data| data.insert_temp(start, banner.focus));
    }
    if body.dragged()
        && let Shown::Ready(texture) = &shown
        && let Some(from) = ui.input(|input| input.pointer.press_origin())
        && let Some(now) = body.interact_pointer_pos()
        && let Some(focus) = ui.data(|data| data.get_temp::<[u8; 2]>(start))
    {
        let spare = overflow(texture.size_vec2(), rect.size());
        let moved = now - from;
        let next = |was: u8, moved: f32, spare: f32| match spare > 0.5 {
            true => (f32::from(was) - moved * 255.0 / spare)
                .round()
                .clamp(0.0, 255.0) as u8,
            false => was,
        };
        let wanted = [
            next(focus[0], moved.x, spare.x),
            next(focus[1], moved.y, spare.y),
        ];
        if wanted != banner.focus {
            change = Some(BannerChange::Focus(wanted));
        }
    }
    body.on_hover_text(t!("home.banner_drag"))
        .on_hover_cursor(CursorIcon::Move);

    let grab = Rect::from_min_max(pos2(rect.left(), rect.bottom() - GRAB), rect.max);
    let edge = ui.interact(grab, id.with("height"), Sense::drag());
    edge.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.banner_height")));
    if edge.dragged()
        && let Some(pointer) = edge.interact_pointer_pos()
    {
        let height = (pointer.y - rect.top()) * 1000.0 / rect.width().min(widest).max(1.0);
        let wanted = height
            .round()
            .clamp(f32::from(MIN_BANNER), f32::from(MAX_BANNER)) as u16;
        if wanted != banner.height {
            change = Some(BannerChange::Height(wanted));
        }
    }
    let color = match edge.hovered() || edge.dragged() {
        true => ui::accent(ui).base,
        false => ui::ON_FILL,
    };
    // Light on a dark rim shows on any picture.
    let bar = Rect::from_center_size(grab.center(), vec2(56.0, 5.0));
    ui.painter()
        .rect_filled(bar.expand(1.5), 3.0, ui::OVERLAY_BUTTON);
    ui.painter().rect_filled(bar, 2.0, color);
    edge.on_hover_text(t!("home.banner_height"))
        .on_hover_cursor(CursorIcon::ResizeVertical);

    // On a dark plate, so they read on any picture.
    let plate = Rect::from_min_size(
        pos2(rect.right() - 2.0 * BUTTON - 22.0, rect.top() + 8.0),
        vec2(2.0 * BUTTON + 14.0, BUTTON + 8.0),
    );
    ui.painter()
        .rect_filled(plate, ui::RADIUS, ui::colors(ui).raised);
    let mut corner = ui.new_child(UiBuilder::new().max_rect(plate.shrink(4.0)).layout(
        eframe::egui::Layout::left_to_right(eframe::egui::Align::Center),
    ));
    let size = Vec2::splat(BUTTON);
    let look = ui::Look::Ghost;
    if ui::icon_button(&mut corner, "🖼", t!("home.banner_replace"), look, size).clicked() {
        change = Some(BannerChange::Replace);
    }
    if ui::icon_button(&mut corner, "🗑", t!("home.banner_remove"), look, size).clicked() {
        change = Some(BannerChange::Remove);
    }
    after_banner(ui, rect);
    change
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_banner_fills_its_strip_without_stretching() {
        let (picture, room) = (vec2(1000., 1000.), vec2(800., 200.));
        let top = cover(picture, room, [128, 0]);
        assert_eq!((top.min.y, top.width(), top.height()), (0., 1., 0.25));
        assert_eq!(cover(picture, room, [128, 255]).max.y, 1.);
        let wide = cover(vec2(4000., 500.), room, [255, 128]);
        assert_eq!((wide.max.x, wide.height()), (1., 1.));
        assert_eq!(overflow(picture, room), vec2(0., 600.));
        assert!(cover(Vec2::ZERO, room, [0, 0]).is_finite());
    }

    #[test]
    fn a_pictures_edge_follows_the_pointer() {
        for align in [Align::Left, Align::Center, Align::Right] {
            let (x, width) = place(100., 400., 50, align);
            let edge = if align == Align::Right { x } else { x + width };
            assert_eq!(dragged_width(100., 400., edge, align), 50, "{align:?}");
        }
        assert_eq!(dragged_width(100., 400., 0., Align::Left), MIN_PICTURE);
        assert_eq!(dragged_width(100., 400., 999., Align::Left), 100);
    }
}
