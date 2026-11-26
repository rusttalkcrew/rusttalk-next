//! The camera of someone sharing their screen, as a small picture on that screen: dragged
//! anywhere, it snaps to the nearest corner; it can pop out into its own tile or shrink
//! to a pill.

use eframe::egui::{
    Align, Align2, CursorIcon, FontId, Id as UiId, Layout, Pos2, Rect, Role, Sense, Stroke,
    StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo,
    text::{LayoutJob, TextWrapping},
    vec2,
};

use crate::{
    server::Id,
    ui::{self, Look},
};

/// Room between the picture and the stream's edge.
pub const MARGIN: f32 = 8.0;
/// The picture's share of the stream's width, and its limits.
const SHARE: f32 = 0.22;
const MIN_WIDTH: f32 = 64.0;
const MAX_WIDTH: f32 = 320.0;
const MIN_STREAM: Vec2 = Vec2::new(160.0, 90.0);
/// At most this part of the stream's height.
const MAX_HEIGHT: f32 = 0.4;
/// Phone cameras are tall, webcams wide: keep the picture between these shapes.
const ASPECTS: (f32, f32) = (3.0 / 4.0, 16.0 / 9.0);
const TOOL: Vec2 = Vec2::splat(24.0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    #[default]
    BottomRight,
}

impl Corner {
    /// Where the corner is, as a share of the free room across and down.
    fn share(self) -> Vec2 {
        match self {
            Corner::TopLeft => vec2(0.0, 0.0),
            Corner::TopRight => vec2(1.0, 0.0),
            Corner::BottomLeft => vec2(0.0, 1.0),
            Corner::BottomRight => vec2(1.0, 1.0),
        }
    }
}

/// How this device shows one person's camera while they share their screen; kept for
/// the app's session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pip {
    pub corner: Corner,
    /// Its own tile beside the screen.
    pub popped: bool,
    /// A pill in its corner; the camera is then neither received nor drawn.
    pub minimized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Corner(Corner),
    PopOut,
    PutBack,
    Minimize,
    Restore,
}

impl Pip {
    pub fn apply(&mut self, change: Change) {
        match change {
            Change::Corner(corner) => self.corner = corner,
            Change::PopOut => {
                self.popped = true;
                self.minimized = false;
            }
            Change::PutBack => self.popped = false,
            Change::Minimize => self.minimized = true,
            Change::Restore => self.minimized = false,
        }
    }
}

/// The corner nearest to `center` in `area`.
pub fn nearest(area: Rect, center: Pos2) -> Corner {
    let left = center.x < area.center().x;
    let top = center.y < area.center().y;
    match (left, top) {
        (true, true) => Corner::TopLeft,
        (false, true) => Corner::TopRight,
        (true, false) => Corner::BottomLeft,
        (false, false) => Corner::BottomRight,
    }
}

/// The picture's size on `stream`, for a camera of shape `aspect` (width / height);
/// `None` when it doesn't fit `area`, the part of the stream it may cover.
pub fn size(stream: Rect, area: Rect, aspect: f32) -> Option<Vec2> {
    let aspect = aspect.clamp(ASPECTS.0, ASPECTS.1);
    let mut width = (stream.width() * SHARE).clamp(MIN_WIDTH, MAX_WIDTH);
    width = width.min(stream.height() * MAX_HEIGHT * aspect);
    let size = vec2(width, width / aspect);
    let room = area.size() - Vec2::splat(2.0 * MARGIN);
    // Strip tiles are too small: the camera would cover most of the screen.
    let roomy = stream.width() >= MIN_STREAM.x && stream.height() >= MIN_STREAM.y;
    (roomy && size.x <= room.x && size.y <= room.y).then_some(size)
}

/// A rect of `size` at `share` of the free room in `area`, `MARGIN` from its edges.
fn at(area: Rect, size: Vec2, share: Vec2) -> Rect {
    let free = (area.size() - size - Vec2::splat(2.0 * MARGIN)).max(Vec2::ZERO);
    Rect::from_min_size(area.min + Vec2::splat(MARGIN) + free * share, size)
}

/// A rect of `size` in `corner` of `area`, `MARGIN` from its edges.
pub fn place(area: Rect, size: Vec2, corner: Corner) -> Rect {
    at(area, size, corner.share())
}

/// The share of the free room a picture whose top left is at `min` sits at.
fn share_at(area: Rect, size: Vec2, min: Pos2) -> Vec2 {
    let free = area.size() - size - Vec2::splat(2.0 * MARGIN);
    let offset = min - area.min - Vec2::splat(MARGIN);
    let share = |offset: f32, free: f32| {
        if free > 0.0 {
            (offset / free).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };
    vec2(share(offset.x, free.x), share(offset.y, free.y))
}

/// The camera of `person` on `stream`, inside `area`: the part clear of the stream's
/// own controls. `paint` draws the camera into the given rect. `pop_out` offers its own
/// tile; without it this is our own floating self view. Returns what was picked.
#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut Ui,
    stream: Rect,
    area: Rect,
    person: Id,
    name: &str,
    pip: Pip,
    aspect: Option<f32>,
    pop_out: bool,
    paint: impl FnOnce(&mut Ui, Rect),
) -> Option<Change> {
    let id = UiId::unique(("camera pip", person));
    if pip.minimized {
        return pill(ui, area, id, pip.corner, name, pop_out);
    }
    let size = size(stream, area, aspect.unwrap_or(4.0 / 3.0))?;
    let ctx = ui.ctx().clone();
    // Where it was last frame, to find what the pointer grabs.
    let last = id.with("rect");
    let hit = ctx.data(|data| data.get_temp::<Rect>(last));
    let hit = hit.unwrap_or_else(|| place(area, size, pip.corner));
    let response = ui.interact(hit, id, Sense::click_and_drag());
    let grab = id.with("grab");
    if response.drag_started()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        ctx.data_mut(|data| data.insert_temp(grab, pointer - hit.min));
    }
    let dragging = response.dragged();
    let mut change = None;
    let share = match (dragging, response.interact_pointer_pos()) {
        (true, Some(pointer)) => {
            let offset = ctx
                .data(|data| data.get_temp::<Vec2>(grab))
                .unwrap_or_default();
            share_at(area, size, pointer - offset)
        }
        _ if response.drag_stopped() => {
            let corner = nearest(area, hit.center());
            change = (corner != pip.corner).then_some(Change::Corner(corner));
            corner.share()
        }
        _ => pip.corner.share(),
    };
    let time = if dragging { 0.0 } else { ui::STATE_SECONDS };
    let share = vec2(
        ctx.animate_value_with_time(id.with("x"), share.x, time),
        ctx.animate_value_with_time(id.with("y"), share.y, time),
    );
    let rect = at(area, size, share);
    // The hit area follows the picture from the next frame on.
    ctx.data_mut(|data| data.insert_temp(last, rect));
    let c = ui::colors(ui);
    ui.painter().rect_filled(rect, ui::RADIUS, c.side);
    paint(ui, rect);
    ui.painter().rect_stroke(
        rect,
        ui::RADIUS,
        Stroke::new(1.0, c.border),
        StrokeKind::Inside,
    );
    let label = if pop_out {
        t!("call.someones_camera", name = name)
    } else {
        t!("call.your_camera").to_owned()
    };
    response.widget_info(|| WidgetInfo::labeled(Role::Image, true, &label));
    let response = if dragging {
        response.on_hover_cursor(CursorIcon::Grabbing)
    } else {
        response.on_hover_cursor(CursorIcon::Grab)
    };
    let (tapped, _) = ui::touch_tools(&response);
    let hovered = ui.rect_contains_pointer(rect) && !ui.input(|i| i.any_touches());
    if (tapped || hovered) && !dragging {
        change = buttons(ui, rect, tapped, pop_out).or(change);
    }
    change
}

/// Pop out (if offered) and minimize, at the picture's top right.
fn buttons(ui: &mut Ui, rect: Rect, tapped: bool, pop_out: bool) -> Option<Change> {
    let room = rect.shrink(ui::SPACE_XS);
    let count = if pop_out { 2.0 } else { 1.0 };
    let tool = if tapped {
        Vec2::splat(
            ui::TOUCH_TARGET
                .min(room.height())
                .min((room.width() - (count - 1.0) * ui::SPACE_XS) / count),
        )
    } else {
        TOOL
    };
    let width = count * tool.x + (count - 1.0) * ui::SPACE_XS;
    if width > room.width() || tool.y > room.height() {
        return None;
    }
    let bar = Rect::from_min_size(
        Pos2::new(room.right() - width, room.top()),
        vec2(width, tool.y),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(bar)
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.spacing_mut().item_spacing.x = ui::SPACE_XS;
    let mut change = None;
    if pop_out
        && ui::icon_button(
            &mut child,
            "↗",
            t!("call.pop_out_camera"),
            Look::Plain,
            tool,
        )
        .clicked()
    {
        change = Some(Change::PopOut);
    }
    if ui::icon_button(
        &mut child,
        "🗕",
        t!("call.minimize_camera"),
        Look::Plain,
        tool,
    )
    .clicked()
    {
        change = Some(Change::Minimize);
    }
    change
}

/// The minimized camera: a pill with the name in its corner; a click brings it back.
fn pill(
    ui: &mut Ui,
    area: Rect,
    id: UiId,
    corner: Corner,
    name: &str,
    theirs: bool,
) -> Option<Change> {
    let height = ui::ICON_BUTTON;
    let pad = ui::SPACE_M;
    let room = area.width() - 2.0 * (MARGIN + pad);
    if room < height || area.height() < height + 2.0 * MARGIN {
        return None;
    }
    let mut job = LayoutJob::simple_singleline(
        format!("📷 {name}"),
        FontId::proportional(ui::TEXT_SMALL),
        ui::OVERLAY_TEXT,
    );
    job.wrap = TextWrapping::truncate_at_width(room.min(160.0));
    let galley = ui.painter().layout_job(job);
    let size = vec2(galley.size().x + 2.0 * pad, height);
    let rect = place(area, size, corner);
    let response = ui
        .interact(rect, id.with("pill"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    let label = if theirs {
        t!("call.show_camera", name = name)
    } else {
        t!("call.show_your_camera").to_owned()
    };
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
    let response = response.on_hover_text(&label);
    let fill = if response.hovered() {
        ui::OVERLAY
    } else {
        ui::OVERLAY_BUTTON
    };
    let painter = ui.painter();
    painter.rect_filled(rect, height / 2.0, fill);
    let text_at = Align2::LEFT_CENTER
        .align_size_within_rect(galley.size(), rect.shrink2(vec2(pad, 0.0)))
        .min;
    painter.galley(text_at, galley, ui::OVERLAY_TEXT);
    ui::focus_ring(ui, &response, ui::RADIUS);
    response.clicked().then_some(Change::Restore)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::pos2;

    fn area(w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(100.0, 50.0), vec2(w, h))
    }

    #[test]
    fn a_drop_snaps_to_the_nearest_corner() {
        let stream = area(800.0, 450.0);
        assert_eq!(nearest(stream, pos2(110.0, 60.0)), Corner::TopLeft);
        assert_eq!(nearest(stream, pos2(880.0, 70.0)), Corner::TopRight);
        assert_eq!(nearest(stream, pos2(150.0, 480.0)), Corner::BottomLeft);
        assert_eq!(nearest(stream, pos2(600.0, 300.0)), Corner::BottomRight);
        // Dropped past the edge still lands on that side.
        assert_eq!(nearest(stream, pos2(-50.0, 900.0)), Corner::BottomLeft);
    }

    #[test]
    fn corners_keep_the_margin_and_follow_a_resize() {
        let size = vec2(160.0, 90.0);
        for stream in [area(800.0, 450.0), area(1920.0, 1080.0), area(390.0, 220.0)] {
            let at = |corner| place(stream, size, corner);
            assert_eq!(at(Corner::TopLeft).min, stream.min + Vec2::splat(MARGIN));
            assert_eq!(
                at(Corner::TopRight).right_top(),
                stream.right_top() + vec2(-MARGIN, MARGIN)
            );
            assert_eq!(
                at(Corner::BottomLeft).left_bottom(),
                stream.left_bottom() + vec2(MARGIN, -MARGIN)
            );
            assert_eq!(
                at(Corner::BottomRight).max,
                stream.max - Vec2::splat(MARGIN)
            );
            // Each corner is where a drop there snaps back to.
            for corner in [
                Corner::TopLeft,
                Corner::TopRight,
                Corner::BottomLeft,
                Corner::BottomRight,
            ] {
                assert_eq!(nearest(stream, at(corner).center()), corner);
            }
        }
    }

    #[test]
    fn a_drag_stays_inside_the_stream() {
        let stream = area(800.0, 450.0);
        let size = vec2(160.0, 90.0);
        assert_eq!(share_at(stream, size, pos2(-500.0, 2000.0)), vec2(0.0, 1.0));
        let middle = share_at(stream, size, stream.center() - size / 2.0);
        assert!((middle - vec2(0.5, 0.5)).length() < 1e-4);
        assert_eq!(
            at(stream, size, vec2(1.0, 1.0)),
            place(stream, size, Corner::BottomRight)
        );
    }

    #[test]
    fn the_picture_fits_its_stream_or_is_left_out() {
        let fit = |w, h, aspect| size(area(w, h), area(w, h), aspect);
        let big = fit(1100.0, 600.0, 16.0 / 9.0).unwrap();
        assert!((big.x - 1100.0 * SHARE).abs() < 1e-3);
        assert!(fit(4000.0, 2200.0, 16.0 / 9.0).unwrap().x <= MAX_WIDTH);
        // A tall phone camera keeps its shape, within limits, and fits the height.
        let tall = fit(800.0, 450.0, 9.0 / 16.0).unwrap();
        assert!((tall.x / tall.y - ASPECTS.0).abs() < 1e-3);
        assert!(tall.y <= 450.0 * MAX_HEIGHT + 1e-3);
        // A small grid tile still has room clear of its buttons and name.
        let tile = area(252.0, 141.0);
        let clear = Rect::from_min_max(pos2(100.0, 86.0), pos2(352.0, 161.0));
        assert!(size(tile, clear, 4.0 / 3.0).is_some());
        // A strip tile is too small for it.
        assert_eq!(fit(120.0, 68.0, 16.0 / 9.0), None);
    }

    #[test]
    fn pop_out_minimize_and_restore() {
        let mut pip = Pip::default();
        pip.apply(Change::Minimize);
        assert!(pip.minimized);
        pip.apply(Change::Restore);
        assert!(!pip.minimized);
        pip.apply(Change::Corner(Corner::TopLeft));
        pip.apply(Change::Minimize);
        // Popping out shows the camera again, and putting it back keeps its corner.
        pip.apply(Change::PopOut);
        assert!(pip.popped && !pip.minimized);
        pip.apply(Change::PutBack);
        assert_eq!(
            pip,
            Pip {
                corner: Corner::TopLeft,
                popped: false,
                minimized: false
            }
        );
    }
}
