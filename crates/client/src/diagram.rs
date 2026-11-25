//! The diagram canvas: shapes, connectors that follow them, selecting, moving,
//! resizing, text, colours, undo and redo, panning and zooming. It draws a
//! `rusttalk_core::diagram::Doc` and turns what the person does into edits; who
//! sends them where is up to the caller (see `call::diagram`).

pub mod drawio;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, CursorIcon, FontId, Id as UiId, Key, Pos2, Rect, Response,
    Sense, Shape as Paint, Stroke, StrokeKind, Ui, UiBuilder, Vec2, pos2, vec2,
};
use rusttalk_core::diagram::{
    COLORS, Doc, Edge, Item, Kind, MAX_TEXT, MIN_SIZE, Op, Shape, new_id,
};

use crate::ui::{self, Look};

/// Edits snap to this grid, in diagram units.
const GRID: f32 = 10.0;
const ZOOM: std::ops::RangeInclusive<f32> = 0.1..=4.0;
/// How near (in points) a click must be to a connector to pick it.
const NEAR: f32 = 6.0;
/// The corner handles of a selected shape, in points.
const HANDLE: f32 = 8.0;
const TOOL: Vec2 = Vec2::splat(30.0);
/// Text size in shapes, in diagram units.
const FONT: f32 = 14.0;
/// Undo steps kept.
const HISTORY: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tool {
    Select,
    Shape(Kind),
    Connect,
}

/// One undoable step: the edits that made it, and those that take it back.
struct Change {
    undo: Vec<Op>,
    redo: Vec<Op>,
}

enum Drag {
    Pan,
    Move {
        id: u64,
        start: Shape,
    },
    Resize {
        id: u64,
        start: Shape,
        corner: usize,
    },
    Connect {
        from: u64,
    },
}

/// How one person looks at and works on a diagram. The view is theirs alone.
pub struct Canvas {
    /// Where the diagram's origin is, from the canvas's top left, and its scale.
    pan: Vec2,
    zoom: f32,
    /// Keep the whole diagram in view while the canvas changes size, until the person
    /// pans or zooms; and the size it was last fitted to.
    fit: bool,
    fitted: Vec2,
    pub tool: Tool,
    /// The colour for new shapes and connectors.
    pub color: u8,
    pub selected: Option<u64>,
    drag: Option<Drag>,
    /// The shape whose text is being edited, and the text so far.
    editing: Option<(u64, String)>,
    undo: Vec<Change>,
    redo: Vec<Change>,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            fit: true,
            fitted: Vec2::ZERO,
            tool: Tool::Select,
            color: 0,
            selected: None,
            drag: None,
            editing: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
}

/// A shape's fill, outline and text colours on this theme.
pub fn colors(ui: &Ui, color: u8) -> (Color32, Color32, Color32) {
    let c = ui::colors(ui);
    match ui::DIAGRAM.get(usize::from(color)) {
        Some((fill, stroke)) if color > 0 => (*fill, *stroke, ui::DIAGRAM_TEXT),
        _ => (c.field, c.strong, c.strong),
    }
}

/// The colour names, in palette order.
pub fn color_name(color: u8) -> &'static str {
    match color {
        1 => t!("diagram.color_blue"),
        2 => t!("diagram.color_green"),
        3 => t!("diagram.color_yellow"),
        4 => t!("diagram.color_orange"),
        5 => t!("diagram.color_red"),
        6 => t!("diagram.color_purple"),
        7 => t!("diagram.color_gray"),
        _ => t!("diagram.color_default"),
    }
}

fn snap(value: f32) -> f32 {
    (value / GRID).round() * GRID
}

fn bounds(shape: &Shape) -> Rect {
    Rect::from_min_size(pos2(shape.x, shape.y), vec2(shape.w, shape.h))
}

fn contains(shape: &Shape, at: Pos2) -> bool {
    let rect = bounds(shape);
    let d = at - rect.center();
    let (hw, hh) = (rect.width() / 2.0, rect.height() / 2.0);
    match shape.kind {
        Kind::Ellipse => (d.x / hw).powi(2) + (d.y / hh).powi(2) <= 1.0,
        Kind::Diamond => d.x.abs() / hw + d.y.abs() / hh <= 1.0,
        Kind::Rect | Kind::Rounded | Kind::Text => rect.contains(at),
    }
}

/// Where a line from the shape's centre towards `toward` leaves its outline.
fn border(shape: &Shape, toward: Pos2) -> Pos2 {
    let rect = bounds(shape);
    let center = rect.center();
    let d = toward - center;
    if d.length() < f32::EPSILON {
        return center;
    }
    let (hw, hh) = (rect.width() / 2.0, rect.height() / 2.0);
    let t = match shape.kind {
        Kind::Ellipse => 1.0 / ((d.x / hw).powi(2) + (d.y / hh).powi(2)).sqrt(),
        Kind::Diamond => 1.0 / (d.x.abs() / hw + d.y.abs() / hh),
        Kind::Rect | Kind::Rounded | Kind::Text => {
            (hw / d.x.abs().max(f32::EPSILON)).min(hh / d.y.abs().max(f32::EPSILON))
        }
    };
    center + d * t.min(1.0)
}

/// A connector's ends, on its shapes' outlines.
pub fn ends(doc: &Doc, edge: &Edge) -> Option<(Pos2, Pos2)> {
    let (from, to) = (doc.shape(edge.from)?, doc.shape(edge.to)?);
    let (a, b) = (bounds(from).center(), bounds(to).center());
    Some((border(from, b), border(to, a)))
}

fn distance_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let t = ((p - a).dot(ab) / ab.length_sq().max(f32::EPSILON)).clamp(0.0, 1.0);
    (a + ab * t).distance(p)
}

/// The topmost shape at `at`.
fn shape_at(doc: &Doc, at: Pos2) -> Option<u64> {
    (doc.shapes.iter().rev())
        .find(|shape| contains(shape, at))
        .map(|shape| shape.id)
}

fn edge_at(doc: &Doc, at: Pos2, near: f32) -> Option<u64> {
    (doc.edges.iter().rev())
        .find(|edge| ends(doc, edge).is_some_and(|(a, b)| distance_to_segment(at, a, b) <= near))
        .map(|edge| edge.id)
}

/// The size a new shape starts at.
fn new_size(kind: Kind) -> Vec2 {
    match kind {
        Kind::Rect | Kind::Rounded => vec2(140.0, 70.0),
        Kind::Ellipse => vec2(120.0, 80.0),
        Kind::Diamond => vec2(120.0, 90.0),
        Kind::Text => vec2(120.0, 40.0),
    }
}

/// Text as a shape may hold it: tabs as spaces, other control characters dropped,
/// cut to the limit at a character boundary.
pub fn clean_text(text: &str) -> String {
    let mut clean = String::new();
    for c in text.chars().map(|c| if c == '\t' { ' ' } else { c }) {
        if c.is_control() && c != '\n' {
            continue;
        }
        if clean.len() + c.len_utf8() > MAX_TEXT {
            break;
        }
        clean.push(c);
    }
    clean
}

impl Canvas {
    fn to_screen(&self, origin: Pos2, at: Pos2) -> Pos2 {
        origin + self.pan + at.to_vec2() * self.zoom
    }

    fn to_diagram(&self, origin: Pos2, at: Pos2) -> Pos2 {
        ((at - origin - self.pan) / self.zoom).to_pos2()
    }

    fn screen_rect(&self, origin: Pos2, shape: &Shape) -> Rect {
        let rect = bounds(shape);
        Rect::from_min_max(
            self.to_screen(origin, rect.min),
            self.to_screen(origin, rect.max),
        )
    }

    /// Show the whole diagram on the next frame, after a new one arrived.
    pub fn refit(&mut self) {
        self.fit = true;
        self.fitted = Vec2::ZERO;
    }

    /// Forget what was selected or being changed, and the history: the diagram changed
    /// under it (a new one, or editing ended).
    pub fn reset(&mut self) {
        self.selected = None;
        self.drag = None;
        self.editing = None;
        self.tool = Tool::Select;
        self.undo.clear();
        self.redo.clear();
    }

    fn fit_to(&mut self, doc: &Doc, area: Rect) {
        let Some(shapes) = doc.shapes.iter().map(bounds).reduce(|a, b| a.union(b)) else {
            self.zoom = 1.0;
            self.pan = area.size() / 2.0 - vec2(200.0, 100.0);
            return;
        };
        let padded = shapes.expand(GRID * 4.0);
        let zoom = (area.width() / padded.width()).min(area.height() / padded.height());
        self.zoom = zoom.clamp(*ZOOM.start(), 1.5);
        self.pan = area.size() / 2.0 - padded.center().to_vec2() * self.zoom;
    }

    fn zoom_at(&mut self, origin: Pos2, at: Pos2, factor: f32) {
        let before = self.to_diagram(origin, at);
        self.zoom = (self.zoom * factor).clamp(*ZOOM.start(), *ZOOM.end());
        self.pan = at - origin - before.to_vec2() * self.zoom;
    }

    /// Record a step, apply it, and pass its edits on.
    fn commit(&mut self, doc: &mut Doc, change: Change, out: &mut Vec<Op>) {
        let applied: Vec<Op> = change
            .redo
            .iter()
            .filter(|op| doc.apply(op))
            .cloned()
            .collect();
        if applied.is_empty() {
            return;
        }
        out.extend(applied);
        self.undo.push(change);
        if self.undo.len() > HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Record a step already shown while dragging; `doc` has it.
    fn record(&mut self, doc: &mut Doc, change: Change, out: &mut Vec<Op>) {
        for op in &change.redo {
            doc.apply(op);
        }
        out.extend(change.redo.iter().cloned());
        self.undo.push(change);
        self.redo.clear();
    }

    pub fn undo(&mut self, doc: &mut Doc, out: &mut Vec<Op>) {
        if let Some(change) = self.undo.pop() {
            out.extend(change.undo.iter().filter(|op| doc.apply(op)).cloned());
            self.redo.push(change);
        }
    }

    pub fn redo(&mut self, doc: &mut Doc, out: &mut Vec<Op>) {
        if let Some(change) = self.redo.pop() {
            out.extend(change.redo.iter().filter(|op| doc.apply(op)).cloned());
            self.undo.push(change);
        }
    }

    /// Delete the selection, with the connectors of a shape.
    pub fn delete(&mut self, doc: &mut Doc, out: &mut Vec<Op>) {
        let Some(id) = self.selected.take() else {
            return;
        };
        let Some(item) = doc.item(id) else {
            return;
        };
        let mut undo = vec![Op::Put { item }];
        undo.extend(
            (doc.edges.iter())
                .filter(|edge| edge.from == id || edge.to == id)
                .map(|edge| Op::Put {
                    item: Item::Edge(edge.clone()),
                }),
        );
        let change = Change {
            undo,
            redo: vec![Op::Delete { id }],
        };
        self.commit(doc, change, out);
    }

    /// Give the selection this colour, and new shapes too.
    pub fn paint(&mut self, doc: &mut Doc, color: u8, out: &mut Vec<Op>) {
        self.color = color.min(COLORS - 1);
        let Some(old) = self.selected.and_then(|id| doc.item(id)) else {
            return;
        };
        let mut new = old.clone();
        match &mut new {
            Item::Shape(shape) => shape.color = self.color,
            Item::Edge(edge) => edge.color = self.color,
        }
        if new != old {
            let change = Change {
                undo: vec![Op::Put { item: old }],
                redo: vec![Op::Put { item: new }],
            };
            self.commit(doc, change, out);
        }
    }

    fn create(&mut self, doc: &mut Doc, kind: Kind, at: Pos2, out: &mut Vec<Op>) {
        let size = new_size(kind);
        let shape = Shape {
            id: new_id(),
            kind,
            x: snap(at.x - size.x / 2.0),
            y: snap(at.y - size.y / 2.0),
            w: size.x,
            h: size.y,
            text: String::new(),
            color: self.color,
        };
        let id = shape.id;
        let change = Change {
            undo: vec![Op::Delete { id }],
            redo: vec![Op::Put {
                item: Item::Shape(shape),
            }],
        };
        self.commit(doc, change, out);
        self.selected = Some(id);
        self.tool = Tool::Select;
        if kind == Kind::Text {
            self.editing = Some((id, String::new()));
        }
    }

    fn connect(&mut self, doc: &mut Doc, from: u64, to: u64, out: &mut Vec<Op>) {
        let edge = Edge {
            id: new_id(),
            from,
            to,
            color: self.color,
        };
        let id = edge.id;
        let change = Change {
            undo: vec![Op::Delete { id }],
            redo: vec![Op::Put {
                item: Item::Edge(edge),
            }],
        };
        self.commit(doc, change, out);
        self.selected = Some(id);
    }

    /// Put the text being edited into its shape.
    fn finish_text(&mut self, doc: &mut Doc, out: &mut Vec<Op>) {
        let Some((id, text)) = self.editing.take() else {
            return;
        };
        let Some(old) = doc.shape(id).cloned() else {
            return;
        };
        let text = clean_text(&text);
        if text == old.text {
            return;
        }
        let new = Shape {
            text,
            ..old.clone()
        };
        let change = Change {
            undo: vec![Op::Put {
                item: Item::Shape(old),
            }],
            redo: vec![Op::Put {
                item: Item::Shape(new),
            }],
        };
        self.commit(doc, change, out);
    }
}

/// Draw `doc` in `rect`. While `active` it can be panned and zoomed, and while also
/// `editable` changed; the edits made this frame are returned, already applied to
/// `doc`. Otherwise it is shown whole. `chrome` is drawn over the toolbar's row:
/// buttons the caller adds after the canvas's own.
pub fn show(
    ui: &mut Ui,
    rect: Rect,
    doc: &mut Doc,
    canvas: &mut Canvas,
    (active, editable): (bool, bool),
    label: &str,
    chrome: impl FnOnce(&mut Ui),
) -> Vec<Op> {
    let mut out = Vec::new();
    let origin = rect.min;
    let id = UiId::unique(("diagram canvas", label));
    if !editable {
        canvas.drag = canvas.drag.take().filter(|drag| matches!(drag, Drag::Pan));
        canvas.editing = None;
        canvas.tool = Tool::Select;
    }
    if !active {
        // Squeezed into the strip: the whole diagram, without touching the person's view.
        let mut whole = Canvas::default();
        whole.fit_to(doc, rect);
        paint(ui, rect, doc, &whole, None);
        let response = ui.interact(rect, id, Sense::hover());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, label));
        return out;
    }
    if canvas.fit && canvas.fitted != rect.size() {
        canvas.fitted = rect.size();
        // Below the toolbar.
        let mut view = rect;
        view.min.y = (view.min.y + TOOL.y + 2.0 * ui::SPACE_S).min(view.max.y - 1.0);
        canvas.fit_to(doc, view);
        canvas.pan.y += view.min.y - rect.min.y;
    }
    let response = ui.interact(rect, id, Sense::click_and_drag());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Group, true, label));
    pointer(ui, &response, origin, doc, canvas, editable, &mut out);
    if editable && canvas.editing.is_none() && response.has_focus() {
        keys(ui, doc, canvas, &mut out);
    }
    let preview = match (&canvas.drag, response.hover_pos()) {
        (Some(Drag::Connect { from }), Some(at)) => Some((*from, at)),
        _ => None,
    };
    paint(ui, rect, doc, canvas, preview);
    if editable {
        text_editor(ui, origin, doc, canvas, &mut out);
    }
    toolbar(ui, rect, doc, canvas, editable, &mut out, chrome);
    out
}

/// Pan, zoom, select, move, resize, connect and place shapes with the pointer.
fn pointer(
    ui: &Ui,
    response: &Response,
    origin: Pos2,
    doc: &mut Doc,
    canvas: &mut Canvas,
    editable: bool,
    out: &mut Vec<Op>,
) {
    let (scroll, pinch, pointer) = ui.input(|i| {
        (
            i.smooth_scroll_delta.y,
            i.zoom_delta(),
            i.pointer.hover_pos(),
        )
    });
    if response.hovered()
        && let Some(at) = pointer
    {
        let factor = pinch * (scroll / 400.0).exp();
        if (factor - 1.0).abs() > f32::EPSILON {
            canvas.zoom_at(origin, at, factor);
            canvas.fit = false;
        }
    }
    if response.drag_started() {
        let press = ui
            .input(|i| i.pointer.press_origin())
            .unwrap_or(response.rect.center());
        let at = canvas.to_diagram(origin, press);
        canvas.drag = Some(Drag::Pan);
        if editable {
            let handle = (canvas.selected.and_then(|id| doc.shape(id))).and_then(|shape| {
                let corners = canvas.screen_rect(origin, shape);
                let corner = corners_of(corners)
                    .iter()
                    .position(|corner| corner.distance(press) <= HANDLE)?;
                Some((shape.clone(), corner))
            });
            let hit = shape_at(doc, at);
            canvas.drag = match (canvas.tool, handle, hit) {
                (Tool::Select, Some((start, corner)), _) => Some(Drag::Resize {
                    id: start.id,
                    start,
                    corner,
                }),
                (Tool::Select, None, Some(id)) => {
                    canvas.selected = Some(id);
                    let start = doc.shape(id).cloned();
                    start.map(|start| Drag::Move { id, start })
                }
                (Tool::Connect, _, Some(from)) => Some(Drag::Connect { from }),
                _ => Some(Drag::Pan),
            };
        }
    }
    if response.dragged() {
        let delta = response.drag_delta();
        let total = ui
            .input(|i| i.pointer.interact_pos().zip(i.pointer.press_origin()))
            .map(|(now, start)| (now - start) / canvas.zoom)
            .unwrap_or_default();
        match &canvas.drag {
            Some(Drag::Pan) => {
                canvas.pan += delta;
                canvas.fit = false;
            }
            Some(Drag::Move { id, start }) => {
                let moved = Shape {
                    x: snap(start.x + total.x),
                    y: snap(start.y + total.y),
                    ..start.clone()
                };
                let id = *id;
                if let Some(shape) = doc.shapes.iter_mut().find(|s| s.id == id) {
                    *shape = moved;
                }
            }
            Some(Drag::Resize { id, start, corner }) => {
                let resized = resize(start, *corner, total);
                let id = *id;
                if let Some(shape) = doc.shapes.iter_mut().find(|s| s.id == id) {
                    *shape = resized;
                }
            }
            Some(Drag::Connect { .. }) | None => {}
        }
        if !matches!(canvas.drag, Some(Drag::Pan) | None) {
            ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
        }
    }
    if response.drag_stopped() {
        match canvas.drag.take() {
            Some(Drag::Move { id, start } | Drag::Resize { id, start, .. }) => {
                // Someone else may have deleted it meanwhile.
                if let Some(now) = doc.shape(id).cloned()
                    && now != start
                {
                    let change = Change {
                        undo: vec![Op::Put {
                            item: Item::Shape(start),
                        }],
                        redo: vec![Op::Put {
                            item: Item::Shape(now),
                        }],
                    };
                    canvas.record(doc, change, out);
                }
            }
            Some(Drag::Connect { from }) => {
                let to = response
                    .interact_pointer_pos()
                    .or(pointer)
                    .and_then(|at| shape_at(doc, canvas.to_diagram(origin, at)));
                if let Some(to) = to.filter(|to| *to != from) {
                    canvas.connect(doc, from, to, out);
                    canvas.tool = Tool::Select;
                }
            }
            Some(Drag::Pan) | None => {}
        }
    }
    if response.clicked() || response.double_clicked() {
        response.request_focus();
        if canvas.editing.is_some() {
            canvas.finish_text(doc, out);
        }
    }
    let Some(at) = response.interact_pointer_pos() else {
        return;
    };
    let at = canvas.to_diagram(origin, at);
    if response.clicked() && editable {
        match canvas.tool {
            Tool::Shape(kind) => canvas.create(doc, kind, at, out),
            Tool::Select | Tool::Connect => {
                canvas.selected =
                    shape_at(doc, at).or_else(|| edge_at(doc, at, NEAR / canvas.zoom));
            }
        }
    }
    if response.double_clicked()
        && editable
        && let Some(id) = shape_at(doc, at)
        && let Some(shape) = doc.shape(id)
    {
        canvas.selected = Some(id);
        canvas.editing = Some((id, shape.text.clone()));
    }
}

/// The corners of a rectangle, clockwise from the top left.
fn corners_of(rect: Rect) -> [Pos2; 4] {
    [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
    ]
}

/// `start` with `corner` dragged by `delta` (diagram units); the opposite corner stays.
fn resize(start: &Shape, corner: usize, delta: Vec2) -> Shape {
    let rect = bounds(start);
    let [mut left, mut top, mut right, mut bottom] =
        [rect.left(), rect.top(), rect.right(), rect.bottom()];
    match corner {
        0 => (left, top) = (left + delta.x, top + delta.y),
        1 => (right, top) = (right + delta.x, top + delta.y),
        2 => (right, bottom) = (right + delta.x, bottom + delta.y),
        _ => (left, bottom) = (left + delta.x, bottom + delta.y),
    }
    let (left, right) = (snap(left.min(right)), snap(left.max(right)));
    let (top, bottom) = (snap(top.min(bottom)), snap(top.max(bottom)));
    Shape {
        x: left,
        y: top,
        w: (right - left).max(GRID.max(MIN_SIZE)),
        h: (bottom - top).max(GRID.max(MIN_SIZE)),
        ..start.clone()
    }
}

/// Delete, undo and redo from the keyboard, and Escape to put the tool down.
fn keys(ui: &Ui, doc: &mut Doc, canvas: &mut Canvas, out: &mut Vec<Op>) {
    let (delete, undo, redo, escape) = ui.input(|i| {
        let command = i.modifiers.command;
        (
            i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace),
            command && !i.modifiers.shift && i.key_pressed(Key::Z),
            command && (i.key_pressed(Key::Y) || (i.modifiers.shift && i.key_pressed(Key::Z))),
            i.key_pressed(Key::Escape),
        )
    });
    if delete {
        canvas.delete(doc, out);
    }
    if undo {
        canvas.undo(doc, out);
    }
    if redo {
        canvas.redo(doc, out);
    }
    if escape {
        canvas.tool = Tool::Select;
        canvas.selected = None;
    }
}

fn paint(ui: &Ui, rect: Rect, doc: &Doc, canvas: &Canvas, preview: Option<(u64, Pos2)>) {
    let c = ui::colors(ui);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, ui::RADIUS, c.main);
    let origin = rect.min;
    // A dot grid, when it isn't too dense to help.
    let step = GRID * 5.0 * canvas.zoom;
    if step >= 12.0 {
        let start = origin + canvas.pan;
        let first = pos2(
            rect.left() + (start.x - rect.left()).rem_euclid(step),
            rect.top() + (start.y - rect.top()).rem_euclid(step),
        );
        let mut x = first.x;
        while x < rect.right() {
            let mut y = first.y;
            while y < rect.bottom() {
                painter.circle_filled(pos2(x, y), 1.0, c.border);
                y += step;
            }
            x += step;
        }
    }
    let zoom = canvas.zoom;
    let line = (1.5 * zoom).clamp(1.0, 3.0);
    for shape in &doc.shapes {
        let area = canvas.screen_rect(origin, shape);
        if !area.intersects(rect) {
            continue;
        }
        let (fill, stroke, text) = colors(ui, shape.color);
        let stroke = Stroke::new(line, stroke);
        match shape.kind {
            Kind::Rect => {
                painter.rect(area, 0, fill, stroke, StrokeKind::Middle);
            }
            Kind::Rounded => {
                let radius = (10.0 * zoom).min(area.height() / 2.0).min(255.0) as u8;
                painter.rect(
                    area,
                    CornerRadius::same(radius),
                    fill,
                    stroke,
                    StrokeKind::Middle,
                );
            }
            Kind::Ellipse => {
                let radius = area.size() / 2.0;
                painter.add(Paint::ellipse_filled(area.center(), radius, fill));
                painter.add(Paint::ellipse_stroke(area.center(), radius, stroke));
            }
            Kind::Diamond => {
                let points = vec![
                    area.center_top(),
                    area.right_center(),
                    area.center_bottom(),
                    area.left_center(),
                ];
                painter.add(Paint::convex_polygon(points, fill, stroke));
            }
            Kind::Text => {}
        }
        let text_color = if shape.kind == Kind::Text && shape.color == 0 {
            c.text
        } else if shape.kind == Kind::Text {
            ui::DIAGRAM[usize::from(shape.color)].1
        } else {
            text
        };
        let editing = canvas
            .editing
            .as_ref()
            .is_some_and(|(id, _)| *id == shape.id);
        // Whole points, so zooming doesn't fill the font atlas with every size.
        let size = (FONT * zoom).round();
        if !shape.text.is_empty() && !editing && size >= 4.0 {
            let wrap = (area.width() - 8.0 * zoom).max(1.0);
            let galley = painter.layout(
                shape.text.clone(),
                FontId::proportional(size),
                text_color,
                wrap,
            );
            let at = area.center() - galley.size() / 2.0;
            painter
                .with_clip_rect(area.intersect(rect))
                .galley(at, galley, text_color);
        }
    }
    let accent = ui::accent(ui).base;
    for edge in &doc.edges {
        let Some((a, b)) = ends(doc, edge) else {
            continue;
        };
        let color = if edge.color == 0 {
            c.strong
        } else {
            ui::DIAGRAM[usize::from(edge.color)].1
        };
        let selected = canvas.selected == Some(edge.id);
        let (a, b) = (canvas.to_screen(origin, a), canvas.to_screen(origin, b));
        if selected {
            painter.line_segment([a, b], Stroke::new(line + 4.0, accent.gamma_multiply(0.4)));
        }
        arrow(&painter, a, b, Stroke::new(line, color), zoom);
    }
    if let Some((from, at)) = preview
        && let Some(shape) = doc.shape(from)
    {
        let start = canvas.to_screen(origin, border(shape, canvas.to_diagram(origin, at)));
        arrow(&painter, start, at, Stroke::new(line, accent), zoom);
    }
    if let Some(shape) = canvas.selected.and_then(|id| doc.shape(id)) {
        let area = canvas.screen_rect(origin, shape);
        painter.rect_stroke(
            area.expand(2.0),
            2,
            Stroke::new(1.5, accent),
            StrokeKind::Outside,
        );
        if canvas.tool == Tool::Select {
            for corner in corners_of(area) {
                let handle = Rect::from_center_size(corner, Vec2::splat(HANDLE));
                painter.rect(
                    handle,
                    2,
                    c.main,
                    Stroke::new(1.5, accent),
                    StrokeKind::Middle,
                );
            }
        }
    }
}

fn arrow(painter: &egui::Painter, a: Pos2, b: Pos2, stroke: Stroke, zoom: f32) {
    let d = b - a;
    let length = d.length();
    if length < 1.0 {
        return;
    }
    let dir = d / length;
    let head = (10.0 * zoom).clamp(6.0, 16.0).min(length);
    let base = b - dir * head;
    let side = vec2(-dir.y, dir.x) * head * 0.5;
    painter.line_segment([a, base], stroke);
    painter.add(Paint::convex_polygon(
        vec![b, base + side, base - side],
        stroke.color,
        Stroke::NONE,
    ));
}

/// The text field over a shape whose text is being edited. Enter or a click elsewhere
/// keeps the text (Shift+Enter starts a new line); Escape drops it.
fn text_editor(ui: &mut Ui, origin: Pos2, doc: &mut Doc, canvas: &mut Canvas, out: &mut Vec<Op>) {
    let Some((id, _)) = &canvas.editing else {
        return;
    };
    let Some(shape) = doc.shape(*id) else {
        // Deleted by someone else meanwhile.
        canvas.editing = None;
        return;
    };
    let area = canvas.screen_rect(origin, shape);
    let size = area.size().max(vec2(160.0, 40.0));
    let place = Rect::from_center_size(area.center(), size);
    let field = UiId::unique("diagram text");
    let escape = ui.input(|i| i.key_pressed(Key::Escape));
    // Taken before the field sees it, so it ends editing instead of starting a line.
    let enter = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));
    let Some((_, text)) = &mut canvas.editing else {
        return;
    };
    let response = ui
        .scope_builder(UiBuilder::new().max_rect(place), |ui| {
            ui.add(
                ui::text_area(text)
                    .id(field)
                    .desired_width(place.width())
                    .desired_rows(2),
            )
        })
        .inner;
    if !response.has_focus() && !response.lost_focus() {
        response.request_focus();
    }
    if escape {
        canvas.editing = None;
    } else if enter || response.lost_focus() {
        canvas.finish_text(doc, out);
    }
}

/// A painted tool icon, so it looks the same on every system font.
#[derive(Clone, Copy)]
enum Icon {
    Select,
    Shape(Kind),
    Connect,
    Undo,
    Redo,
    Fit,
}

fn icon_button(ui: &mut Ui, icon: Icon, tip: &str, on: bool) -> Response {
    let look = if on {
        Look::On(ui::accent(ui).base)
    } else {
        Look::Ghost
    };
    let response = ui::icon_button(ui, "", tip, look, TOOL);
    let c = ui::colors(ui);
    let color = if on { ui::ON_FILL } else { c.text };
    let stroke = Stroke::new(1.5, color);
    let area = Rect::from_center_size(response.rect.center(), Vec2::splat(16.0));
    let painter = ui.painter();
    match icon {
        Icon::Shape(Kind::Rect) => {
            painter.rect_stroke(area.shrink2(vec2(0.0, 3.0)), 0, stroke, StrokeKind::Middle);
        }
        Icon::Shape(Kind::Rounded) => {
            painter.rect_stroke(area.shrink2(vec2(0.0, 3.0)), 4, stroke, StrokeKind::Middle);
        }
        Icon::Shape(Kind::Ellipse) => {
            let radius = vec2(8.0, 6.0);
            painter.add(Paint::ellipse_stroke(area.center(), radius, stroke));
        }
        Icon::Shape(Kind::Diamond) => {
            let points = vec![
                area.center_top(),
                area.right_center(),
                area.center_bottom(),
                area.left_center(),
                area.center_top(),
            ];
            painter.add(Paint::line(points, stroke));
        }
        Icon::Shape(Kind::Text) => {
            painter.text(
                area.center(),
                Align2::CENTER_CENTER,
                "T",
                FontId::proportional(ui::TEXT_ICON),
                color,
            );
        }
        Icon::Select => {
            let tip = area.left_top() + vec2(3.0, 1.0);
            let points = vec![
                tip,
                tip + vec2(0.0, 13.0),
                tip + vec2(3.5, 9.5),
                tip + vec2(9.0, 9.5),
            ];
            painter.add(Paint::convex_polygon(points, color, Stroke::NONE));
        }
        Icon::Connect => arrow(painter, area.left_bottom(), area.right_top(), stroke, 1.0),
        Icon::Undo | Icon::Redo => {
            // Undo's arrow ends at the left, redo's at the right.
            let flip = if matches!(icon, Icon::Undo) {
                1.0
            } else {
                -1.0
            };
            let center = area.center() + vec2(0.0, 2.0);
            let points: Vec<Pos2> = (0..=8)
                .map(|step| {
                    let angle = std::f32::consts::PI * (1.0 + step as f32 / 8.0);
                    center + vec2(flip * -angle.cos(), angle.sin()) * 6.0
                })
                .collect();
            let end = points[points.len() - 1];
            painter.add(Paint::line(points, stroke));
            painter.add(Paint::convex_polygon(
                vec![
                    end + vec2(-3.0, 0.0),
                    end + vec2(3.0, 0.0),
                    end + vec2(0.0, 4.0),
                ],
                color,
                Stroke::NONE,
            ));
        }
        Icon::Fit => {
            for corner in corners_of(area.shrink(1.0)) {
                let inward = (area.center() - corner).normalized() * 4.0;
                painter.line_segment([corner, corner + vec2(inward.x, 0.0)], stroke);
                painter.line_segment([corner, corner + vec2(0.0, inward.y)], stroke);
            }
        }
    }
    response
}

/// Start a new row of tools when the next one won't fit: a disabled button is laid
/// out in a scope of its own, which doesn't wrap by itself.
fn wrap(ui: &mut Ui) {
    if ui.available_size_before_wrap().x < TOOL.x {
        ui.end_row();
    }
}

/// The tools along the top: what to draw, colour, delete, undo, redo and fit (only
/// fit without the right to edit), then the caller's `chrome`. Wraps on a narrow tile.
fn toolbar(
    ui: &mut Ui,
    rect: Rect,
    doc: &mut Doc,
    canvas: &mut Canvas,
    editable: bool,
    out: &mut Vec<Op>,
    chrome: impl FnOnce(&mut Ui),
) {
    let area = rect.shrink(ui::SPACE_S);
    // A raised panel behind the tools, sized once they are laid out.
    let painter = ui.painter_at(rect);
    let panel = painter.add(Paint::Noop);
    // One row high: further rows grow below it, each as high as a tool.
    let row = Rect::from_min_size(area.min, vec2(area.width(), TOOL.y));
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(row)
            .layout(egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true)),
    );
    child.spacing_mut().item_spacing = Vec2::splat(ui::SPACE_XS);
    let ui = &mut child;
    if editable {
        let tools = [
            (Tool::Select, Icon::Select, t!("diagram.select")),
            (
                Tool::Shape(Kind::Rect),
                Icon::Shape(Kind::Rect),
                t!("diagram.rectangle"),
            ),
            (
                Tool::Shape(Kind::Rounded),
                Icon::Shape(Kind::Rounded),
                t!("diagram.rounded"),
            ),
            (
                Tool::Shape(Kind::Ellipse),
                Icon::Shape(Kind::Ellipse),
                t!("diagram.ellipse"),
            ),
            (
                Tool::Shape(Kind::Diamond),
                Icon::Shape(Kind::Diamond),
                t!("diagram.diamond"),
            ),
            (
                Tool::Shape(Kind::Text),
                Icon::Shape(Kind::Text),
                t!("diagram.text"),
            ),
            (Tool::Connect, Icon::Connect, t!("diagram.connect")),
        ];
        for (tool, icon, tip) in tools {
            if icon_button(ui, icon, tip, canvas.tool == tool).clicked() {
                canvas.finish_text(doc, out);
                canvas.tool = tool;
            }
        }
        let (fill, _, _) = colors(ui, canvas.color);
        let button = ui::icon_button(ui, "", t!("diagram.color"), Look::Ghost, TOOL);
        ui.painter().circle(
            button.rect.center(),
            7.0,
            fill,
            Stroke::new(1.5, ui::colors(ui).text),
        );
        ui::menu(&button, |m| {
            m.ui().horizontal_wrapped(|ui| {
                for color in 0..COLORS {
                    let (fill, _, _) = colors(ui, color);
                    let picked = canvas.color == color;
                    if ui::color_swatch(ui, Some(fill), picked, color_name(color)).clicked() {
                        canvas.paint(doc, color, out);
                    }
                }
            });
        });
        let selected = canvas.selected.is_some();
        wrap(ui);
        let delete = ui.add_enabled_ui(selected, |ui| {
            ui::icon_button(ui, "🗑", t!("diagram.delete"), Look::Ghost, TOOL)
        });
        if delete.inner.clicked() {
            canvas.delete(doc, out);
        }
        wrap(ui);
        let undo = ui.add_enabled_ui(!canvas.undo.is_empty(), |ui| {
            icon_button(ui, Icon::Undo, t!("diagram.undo"), false)
        });
        if undo.inner.clicked() {
            canvas.undo(doc, out);
        }
        wrap(ui);
        let redo = ui.add_enabled_ui(!canvas.redo.is_empty(), |ui| {
            icon_button(ui, Icon::Redo, t!("diagram.redo"), false)
        });
        if redo.inner.clicked() {
            canvas.redo(doc, out);
        }
    }
    if icon_button(ui, Icon::Fit, t!("diagram.fit"), false).clicked() {
        canvas.refit();
    }
    chrome(ui);
    let c = ui::colors(ui);
    let used = ui.min_rect().expand(ui::SPACE_XS);
    painter.set(
        panel,
        Paint::Rect(egui::epaint::RectShape::new(
            used,
            ui::RADIUS,
            c.side,
            Stroke::new(1.0, c.border),
            StrokeKind::Inside,
        )),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(id: u64, kind: Kind) -> Shape {
        Shape {
            id,
            kind,
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 50.0,
            text: String::new(),
            color: 0,
        }
    }

    #[test]
    fn connectors_leave_each_shape_at_its_outline() {
        let toward = pos2(500.0, 25.0);
        assert_eq!(border(&shape(1, Kind::Rect), toward), pos2(100.0, 25.0));
        assert_eq!(border(&shape(1, Kind::Ellipse), toward), pos2(100.0, 25.0));
        let corner = border(&shape(1, Kind::Diamond), pos2(50.0, 500.0));
        assert_eq!(corner, pos2(50.0, 50.0));
        // Halfway along the diamond's edge, not the box's corner.
        let diagonal = border(&shape(1, Kind::Diamond), pos2(100.0, 50.0));
        assert!((diagonal.x - 75.0).abs() < 0.01 && (diagonal.y - 37.5).abs() < 0.01);
    }

    #[test]
    fn undo_and_redo_take_back_and_repeat_edits() {
        let mut doc = Doc::default();
        let mut canvas = Canvas::default();
        let mut out = Vec::new();
        canvas.create(&mut doc, Kind::Rect, pos2(0.0, 0.0), &mut out);
        canvas.create(&mut doc, Kind::Ellipse, pos2(300.0, 0.0), &mut out);
        let (a, b) = (doc.shapes[0].id, doc.shapes[1].id);
        canvas.connect(&mut doc, a, b, &mut out);
        assert_eq!(doc.edges.len(), 1);
        let full = doc.clone();
        // Deleting a shape takes its connector; undo brings both back.
        canvas.selected = Some(a);
        canvas.delete(&mut doc, &mut out);
        assert_eq!((doc.shapes.len(), doc.edges.len()), (1, 0));
        canvas.undo(&mut doc, &mut out);
        assert_eq!((doc.shapes.len(), doc.edges.len()), (2, 1));
        canvas.redo(&mut doc, &mut out);
        assert_eq!(doc.edges.len(), 0);
        canvas.undo(&mut doc, &mut out);
        // Every edit went out to be sent, and replaying them gives the same diagram.
        let mut replayed = Doc::default();
        for op in &out {
            replayed.apply(op);
        }
        assert_eq!(replayed.len(), doc.len());
        assert_eq!(full.edges[0], doc.edges[0]);
    }

    #[test]
    fn text_is_cleaned_and_cut() {
        assert_eq!(clean_text("a\tb\u{7}\nc"), "a b\nc");
        let long = "é".repeat(MAX_TEXT);
        let cut = clean_text(&long);
        assert!(cut.len() <= MAX_TEXT && cut.chars().all(|c| c == 'é'));
    }

    #[test]
    fn resizing_keeps_the_opposite_corner_and_a_minimum_size() {
        let start = shape(1, Kind::Rect);
        let bigger = resize(&start, 2, vec2(40.0, 20.0));
        assert_eq!(
            (bigger.x, bigger.y, bigger.w, bigger.h),
            (0.0, 0.0, 140.0, 70.0)
        );
        let flat = resize(&start, 0, vec2(0.0, 80.0));
        assert!(flat.h >= MIN_SIZE);
    }

    type State = (Doc, Canvas, Vec<Op>);
    type Harness = egui_kittest::Harness<'static, State>;

    fn harness(doc: Doc) -> Harness {
        let mut h = egui_kittest::Harness::builder()
            .with_size(vec2(600.0, 400.0))
            // Frames as often as a screen draws them, so two clicks make a double-click.
            .with_step_dt(1.0 / 60.0)
            .build_ui_state(
                |ui, (doc, canvas, out): &mut State| {
                    let rect = ui.max_rect();
                    out.extend(show(ui, rect, doc, canvas, (true, true), "test", |_| {}));
                },
                (doc, Canvas::default(), Vec::new()),
            );
        ui::install(&h.ctx);
        h.run_steps(3);
        h
    }

    fn at(h: &Harness, x: f32, y: f32) -> Pos2 {
        h.state().1.to_screen(Pos2::ZERO, pos2(x, y))
    }

    fn press(h: &mut Harness, at: Pos2, pressed: bool) {
        h.event(egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
        h.run_steps(1);
    }

    fn drag(h: &mut Harness, from: Pos2, to: Pos2) {
        h.hover_at(from);
        h.run_steps(1);
        press(h, from, true);
        for step in 1..=5 {
            h.hover_at(from + (to - from) * step as f32 / 5.0);
            h.run_steps(1);
        }
        press(h, to, false);
        h.run_steps(1);
    }

    /// The pointer and keyboard as someone uses them: move a shape, connect two, give
    /// one text, delete and undo.
    #[test]
    fn the_canvas_turns_pointer_and_keys_into_edits() {
        let mut doc = Doc::default();
        for (id, x) in [(1, 0.0), (2, 300.0)] {
            doc.apply(&Op::Put {
                item: Item::Shape(Shape {
                    x,
                    ..shape(id, Kind::Rect)
                }),
            });
        }
        let mut h = harness(doc);
        // Drag the first shape down.
        let zoom = h.state().1.zoom;
        let from = at(&h, 50.0, 25.0);
        drag(&mut h, from, from + vec2(0.0, 100.0 * zoom));
        let moved = h.state().0.shape(1).unwrap().clone();
        assert_eq!((moved.x, moved.y), (0.0, 100.0));
        let sent = std::mem::take(&mut h.state_mut().2);
        assert_eq!(
            sent,
            [Op::Put {
                item: Item::Shape(moved)
            }]
        );
        // The arrow tool: drag from one shape to the other.
        h.state_mut().1.tool = Tool::Connect;
        let (from, to) = (at(&h, 50.0, 125.0), at(&h, 350.0, 25.0));
        drag(&mut h, from, to);
        let edge = h.state().0.edges[0].clone();
        assert_eq!((edge.from, edge.to), (1, 2));
        assert_eq!(h.state().1.tool, Tool::Select);
        // Double-click the second shape, type, Enter.
        let second = at(&h, 350.0, 25.0);
        h.hover_at(second);
        h.run_steps(1);
        for _ in 0..2 {
            press(&mut h, second, true);
            press(&mut h, second, false);
        }
        h.run_steps(2);
        assert!(h.state().1.editing.is_some());
        h.event(egui::Event::Text("Ship it".into()));
        h.run_steps(1);
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert_eq!(h.state().0.shape(2).unwrap().text, "Ship it");
        assert!(h.state().1.editing.is_none());
        // Select the first shape and press Delete: its arrow goes too. Undo brings both.
        let first = at(&h, 50.0, 125.0);
        h.hover_at(first);
        press(&mut h, first, true);
        press(&mut h, first, false);
        h.run_steps(1);
        assert_eq!(h.state().1.selected, Some(1));
        h.key_press(Key::Delete);
        h.run_steps(1);
        let doc = &h.state().0;
        assert_eq!((doc.shapes.len(), doc.edges.len()), (1, 0));
        h.key_press_modifiers(egui::Modifiers::COMMAND, Key::Z);
        h.run_steps(1);
        let doc = &h.state().0;
        assert_eq!((doc.shapes.len(), doc.edges.len()), (2, 1));
    }
}
