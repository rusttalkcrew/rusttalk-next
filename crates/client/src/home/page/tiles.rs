//! A page's tiles on screen: one surface with a title on top. While editing, a tile is
//! dragged anywhere to move it, by its right edge to resize it, and clicked to write.

use eframe::egui::{
    Align, Align2, Area, Color32, CursorIcon, FontId, Frame, Id as UiId, Key as Pressed,
    Layout as UiLayout, Margin, Order, Pos2, Rect, Role, Sense, Stroke, StrokeKind, TextBuffer,
    TextEdit, Ui, UiBuilder, Vec2, WidgetInfo, pos2, vec2,
};
use rusttalk_core::pages::{Block, GRID, Layout, MAX_TITLE, MIN_WIDTH, Tile};

use super::{
    editor::{Change, Editing, Editor, Field, Focus, Key, Place, Step},
    markup,
    pictures::{self, Art},
};
use crate::{
    emoji,
    home::{markdown, mentions},
    server::{Directory, Id},
    ui,
};

/// A tile narrower than this stacks its row.
const MIN_TILE: f32 = 160.0;
/// A wide window centres the page rather than stretching it.
pub(super) const MAX_PAGE: f32 = 1100.0;
/// Around a tile's content, the same for reading and writing so nothing jumps.
const PAD: f32 = 10.0;
/// Between tiles.
const GAP: f32 = 12.0;
/// Where a tile's handle and menu sit while editing.
const STRIP: f32 = 22.0;
/// The strip between rows that adds a tile.
const BETWEEN: f32 = 20.0;
/// How near a row's top or bottom a drop makes a row of its own.
const ROW_EDGE: f32 = 18.0;
/// The strip on a tile's right that resizes it.
const GRAB: f32 = 12.0;
const TITLE: f32 = 32.0;
const LIFTED_HEIGHT: f32 = 160.0;
const LIFTED_WIDTH: f32 = 260.0;
const GRIP: &str = "≡";
const MORE: &str = "…";

/// Whose page it is, for mentions and emoji.
#[derive(Clone, Copy)]
pub(super) struct Reader<'a> {
    pub dir: &'a Directory,
    pub channel: Id,
    pub person: Id,
}

impl Reader<'_> {
    /// `opens`: a click on a picture opens it large.
    fn show(&self, ui: &mut Ui, art: &mut Art, tile: &Tile, opens: bool) {
        match tile {
            Tile::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        Block::Markdown(text) => self.text(ui, text),
                        Block::Picture(picture) => pictures::picture(ui, art, picture, opens),
                    }
                }
            }
            Tile::Unknown { .. } => {
                ui.weak(t!("home.tile_unknown"));
            }
        }
    }

    fn text(&self, ui: &mut Ui, text: &str) {
        let doc = markdown::cached(ui, text);
        let mention = mentions::Message::of_text(doc, self.dir, self.channel, self.person);
        mention.doc.show(ui, self.dir, &mention, false, false);
    }
}

pub(super) fn field(channel: Id, field: Field) -> UiId {
    UiId::unique(("page-text", channel, field))
}

pub(super) fn title_field(channel: Id) -> UiId {
    UiId::unique(("page-title", channel))
}

fn blank(blocks: &[Block]) -> bool {
    (blocks.iter()).all(|block| matches!(block, Block::Markdown(text) if text.trim().is_empty()))
}

fn title_font() -> FontId {
    FontId::new(TITLE, emoji::family(true))
}

/// The page's one background, over everything the scroll area shows.
pub(super) fn surface(ui: &Ui) {
    let clip = ui.clip_rect();
    ui.painter()
        .rect_filled(clip, ui::RADIUS_WINDOW, ui::colors(ui).side);
}

/// The middle of the pane, where the page is.
pub(super) fn column(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    let area = ui.available_rect_before_wrap();
    let outer = ui::SPACE_L;
    let width = (area.width() - 2.0 * outer).min(MAX_PAGE);
    let left = area.left() + (area.width() - width) / 2.0;
    let bounds = Rect::from_min_size(pos2(left, area.top() + outer), vec2(width, f32::INFINITY));
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(bounds)
            .layout(UiLayout::top_down(Align::Min)),
    );
    child.set_width(width);
    add(&mut child);
    let used = child.min_rect().height() + 2.0 * outer;
    ui.advance_cursor_after_rect(Rect::from_min_size(area.min, vec2(area.width(), used)));
}

/// Where a row's tiles go.
#[derive(Clone, Copy)]
struct Grid {
    left: f32,
    width: f32,
    /// A twelfth, with its gap.
    unit: f32,
    stacked: bool,
}

impl Grid {
    fn new(left: f32, width: f32, widths: &[usize]) -> Self {
        let unit = (width + GAP) / GRID as f32;
        let stacked =
            (widths.iter()).any(|&span| span < GRID && span as f32 * unit - GAP < MIN_TILE);
        Self {
            left,
            width,
            unit,
            stacked,
        }
    }

    /// Left and width of a tile after `before` twelfths.
    fn span(&self, before: usize, span: usize) -> (f32, f32) {
        match self.stacked {
            true => (self.left, self.width),
            false => (
                self.left + before as f32 * self.unit,
                span as f32 * self.unit - GAP,
            ),
        }
    }
}

/// One cell of a row. Its `id` keeps focus and menus when the tile moves.
fn cell(ui: &mut Ui, id: UiId, at: Pos2, width: f32, add: impl FnOnce(&mut Ui)) -> Rect {
    let bounds = Rect::from_min_size(at, vec2(width, f32::INFINITY));
    let mut child = ui.new_child(
        UiBuilder::new()
            .scope_id(id)
            .max_rect(bounds)
            .layout(UiLayout::top_down(Align::Min)),
    );
    // A long word or code line stays out of the next tile.
    let mut clip = ui.clip_rect();
    clip.min.x = clip.min.x.max(at.x - GAP / 2.0);
    clip.max.x = clip.max.x.min(at.x + width + GAP / 2.0);
    child.set_clip_rect(clip);
    child.set_width(width);
    add(&mut child);
    Rect::from_min_size(at, vec2(width, child.min_rect().height()))
}

fn padded<R>(ui: &mut Ui, top: f32, add: impl FnOnce(&mut Ui) -> R) -> R {
    let margin = Margin {
        left: PAD as i8,
        right: PAD as i8,
        top: top as i8,
        bottom: PAD as i8,
    };
    Frame::NONE
        .inner_margin(margin)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// The small line under the page, in line with the text.
pub(super) fn caption(ui: &mut Ui, text: &str) {
    padded(ui, 0.0, |ui| ui.label(ui::caption(text)));
}

/// The page as readers see it.
pub(super) fn show(ui: &mut Ui, reader: Reader, art: &mut Art, layout: &Layout) {
    if !layout.title.is_empty() {
        padded(ui, 0.0, |ui| {
            ui.label(
                eframe::egui::RichText::new(&layout.title)
                    .font(title_font())
                    .color(ui::colors(ui).strong),
            );
        });
    }
    let area = ui.max_rect();
    for (index, row) in layout.rows.iter().enumerate() {
        let widths: Vec<usize> = row.tiles.iter().map(|placed| placed.width).collect();
        let grid = Grid::new(area.left(), area.width(), &widths);
        let top = ui.cursor().top();
        let (mut before, mut y, mut bottom) = (0, top, top);
        for (at, placed) in row.tiles.iter().enumerate() {
            let (left, width) = grid.span(before, placed.width);
            let id = UiId::unique(("page-read", reader.channel, index, at));
            let rect = cell(ui, id, pos2(left, y), width, |ui| {
                padded(ui, PAD, |ui| reader.show(ui, art, &placed.tile, true));
            });
            before += placed.width;
            bottom = f32::max(bottom, rect.bottom());
            if grid.stacked {
                y = rect.bottom();
            }
        }
        ui.advance_cursor_after_rect(Rect::from_min_max(
            pos2(area.left(), top),
            pos2(area.right(), bottom),
        ));
    }
}

/// A row as laid out, for finding where a drag points.
struct Laid {
    rect: Rect,
    tiles: Vec<(Key, Rect)>,
    takes_more: bool,
    stacked: bool,
}

/// Where dropping `dragged` at `at` puts it; `None` outside the page or on itself.
fn target(rows: &[Laid], dragged: Key, at: Pos2) -> Option<Place> {
    let (first, last) = (rows.first()?, rows.last()?);
    if at.x < first.rect.left() || at.x > first.rect.right() {
        return None;
    }
    if at.y < first.rect.top() {
        return Some(Place::NewRow(0));
    }
    if at.y >= last.rect.bottom() {
        return Some(Place::NewRow(rows.len()));
    }
    let index = rows.iter().position(|row| at.y < row.rect.bottom())?;
    let row = &rows[index];
    let edge = ROW_EDGE.min(row.rect.height() / 4.0);
    if at.y < row.rect.top() + edge {
        return Some(Place::NewRow(index));
    }
    if at.y > row.rect.bottom() - edge {
        return Some(Place::NewRow(index + 1));
    }
    let own = row.tiles.iter().position(|(key, _)| *key == dragged);
    if own.is_some() || row.takes_more {
        let before = (row.tiles.iter())
            .filter(|(_, rect)| match row.stacked {
                true => rect.center().y < at.y,
                false => rect.center().x < at.x,
            })
            .count();
        // Either side of itself is where it already is.
        if own.is_some_and(|own| before == own || before == own + 1) {
            return None;
        }
        return Some(Place::Into(index, before));
    }
    (row.tiles.iter())
        .find(|(key, rect)| *key != dragged && rect.contains(at))
        .map(|(key, _)| Place::Swap(*key))
}

fn landing(ui: &Ui, rows: &[Laid], place: Place) {
    let color = ui::accent(ui).base;
    let painter = ui.painter();
    let bar = |from: Pos2, to: Pos2| {
        let rect = Rect::from_two_pos(from, to).expand(2.5);
        painter.rect_filled(rect, 2.5, color);
    };
    match place {
        Place::NewRow(at) => {
            let Some(last) = rows.last() else {
                return;
            };
            let y = match rows.get(at) {
                Some(row) => row.rect.top() - BETWEEN / 2.0,
                None => last.rect.bottom() + BETWEEN / 2.0,
            };
            bar(pos2(last.rect.left(), y), pos2(last.rect.right(), y));
        }
        Place::Into(row, before) => {
            let row = &rows[row];
            let (rect, after) = match row.tiles.get(before) {
                Some((_, rect)) => (*rect, false),
                None => match row.tiles.last() {
                    Some((_, rect)) => (*rect, true),
                    None => return,
                },
            };
            if row.stacked {
                let y = if after { rect.bottom() } else { rect.top() };
                bar(pos2(rect.left(), y), pos2(rect.right(), y));
            } else {
                let x = match after {
                    true => rect.right() + GAP / 2.0,
                    false => rect.left() - GAP / 2.0,
                };
                bar(pos2(x, row.rect.top()), pos2(x, row.rect.bottom()));
            }
        }
        Place::Swap(key) => {
            if let Some((_, rect)) = (rows.iter())
                .flat_map(|row| &row.tiles)
                .find(|(other, _)| *other == key)
            {
                painter.rect_filled(*rect, ui::RADIUS, color.gamma_multiply(0.18));
                painter.rect_stroke(
                    *rect,
                    ui::RADIUS,
                    Stroke::new(2.0, color),
                    StrokeKind::Inside,
                );
            }
        }
    }
}

/// A tile lifted to move it, or held by its edge.
#[derive(Clone, Copy)]
enum Drag {
    Move { key: Key, width: f32 },
    Resize { key: Key },
}

/// What one tile needs to know of the page.
struct Moves {
    steps: [(Step, Option<Place>); 4],
    beside: Option<Place>,
}

/// What happened in the tiles this frame.
#[derive(Default)]
struct Done {
    change: Option<Change>,
    started: Option<Drag>,
    /// The running drag's handle was seen: whether it was let go.
    released: Option<bool>,
    closed: bool,
    /// The block whose text has the keyboard.
    at: Option<usize>,
}

impl Done {
    fn held(&mut self, response: &eframe::egui::Response) {
        // The whole tile and its handle both lift it: either one still held counts.
        let released = response.drag_stopped();
        if released || response.dragged() {
            self.released = Some(released);
        }
    }
}

/// The page in place, ready to change. `viewport` is what of it the scroll area shows.
pub(super) fn edit(
    ui: &mut Ui,
    reader: Reader,
    art: &mut Art,
    editor: &mut Editor,
    viewport: Rect,
) {
    let drag_id = UiId::unique(("page-drag", reader.channel));
    let mut drag: Option<Drag> = ui.data(|data| data.get_temp(drag_id));
    let pointer = ui.ctx().pointer_interact_pos();
    let clip = ui.clip_rect();
    let area = ui.max_rect();
    let origin = area.top();
    let full = editor.full();
    let focus = editor.focus.take();
    let mut done = Done::default();
    let mut laid = Vec::with_capacity(editor.rows.len());

    // The strips between rows are all the room there is between them.
    let lines = std::mem::replace(&mut ui.spacing_mut().item_spacing.y, 0.0);
    let title = padded(ui, 0.0, |ui| {
        ui.add(
            TextEdit::singleline(&mut editor.title)
                .id(title_field(reader.channel))
                .frame(Frame::NONE)
                .font(title_font())
                .text_color(ui::colors(ui).strong)
                .char_limit(MAX_TITLE)
                .hint_text(
                    eframe::egui::RichText::new(t!("home.page_title_hint")).font(title_font()),
                )
                .desired_width(f32::INFINITY),
        )
    });
    if focus == Some(Focus::Title) {
        title.request_focus();
    }
    if title.lost_focus()
        && ui.input(|input| input.key_pressed(Pressed::Enter))
        && let Some(first) = editor.rows.first().and_then(|row| row.tiles.first())
    {
        done.change = Some(Change::Write(Some(first.key)));
    }

    let moves: Vec<Vec<Moves>> = (editor.rows.iter().enumerate())
        .map(|(index, row)| {
            (row.tiles.iter().enumerate())
                .map(|(at, tile)| Moves {
                    steps: [Step::Up, Step::Down, Step::Left, Step::Right]
                        .map(|step| (step, editor.step(tile.key, step))),
                    beside: (!full && row.takes_more()).then_some(Place::Into(index, at + 1)),
                })
                .collect()
        })
        .collect();
    let writing = editor.writing;
    let (active, caret) = (editor.block, editor.caret);
    for (index, (held, moves)) in editor.rows.iter_mut().zip(&moves).enumerate() {
        let gap_id = UiId::unique(("page-between", reader.channel, held.tiles[0].key));
        if between(ui, gap_id, full) {
            done.change = Some(Change::Add(Place::NewRow(index)));
        }
        let takes_more = held.takes_more();
        let room = held.room();
        let widths: Vec<usize> = held.tiles.iter().map(|tile| tile.width).collect();
        let grid = Grid::new(area.left(), area.width(), &widths);
        let top = ui.cursor().top();
        let (mut before, mut y, mut bottom) = (0, top, top);
        let mut tiles = Vec::with_capacity(widths.len());
        for (at, editing) in held.tiles.iter_mut().enumerate() {
            let key = editing.key;
            let (left, width) = grid.span(before, widths[at]);
            let id = UiId::unique(("page-tile", reader.channel, key));
            // Slide to a new place, in the page's own coordinates so scrolling doesn't lag.
            let seconds = match drag {
                Some(Drag::Resize { .. }) => 0.0,
                _ => ui::SLIDE_SECONDS,
            };
            let slide =
                |axis: &str, to: f32| ui.ctx().animate_value_with_time(id.with(axis), to, seconds);
            let shown = pos2(
                area.left() + slide("x", left - area.left()),
                origin + slide("y", y - origin),
            );
            let tile = Shown {
                reader,
                writing: writing == Some(key),
                focus: focus == Some(Focus::Tile(key)),
                lifted: matches!(drag, Some(Drag::Move { key: lifted, .. }) if lifted == key),
                lines,
                active,
                caret,
                moves: &moves[at],
                id,
            };
            let drawn = cell(ui, id, shown, width, |ui| {
                tile.show(ui, art, editing, &mut done)
            });
            let rect = Rect::from_min_size(pos2(left, y), drawn.size());
            if !grid.stacked {
                edge(
                    ui,
                    &grid,
                    (before, widths[at]),
                    drawn,
                    id,
                    key,
                    &mut drag,
                    &mut done,
                );
            }
            tiles.push((key, rect));
            before += widths[at];
            bottom = f32::max(bottom, rect.bottom());
            if grid.stacked {
                y = rect.bottom() + GAP;
            }
        }
        if !grid.stacked && takes_more && room >= MIN_WIDTH && !full && drag.is_none() {
            let (left, width) = grid.span(before, room);
            let slot = Rect::from_min_max(pos2(left, top), pos2(left + width, bottom));
            if empty_slot(ui, slot, gap_id.with("slot")) {
                done.change = Some(Change::Add(Place::Into(index, widths.len())));
            }
        }
        let rect = Rect::from_min_max(pos2(area.left(), top), pos2(area.right(), bottom));
        ui.advance_cursor_after_rect(rect);
        laid.push(Laid {
            rect,
            tiles,
            takes_more,
            stacked: grid.stacked,
        });
    }
    ui.add_space(ui::SPACE_M);
    if full {
        ui.weak(t!("home.tiles_full"));
    } else if ui::secondary(ui, &format!("+ {}", t!("home.tile_add"))).clicked() {
        done.change = Some(Change::Add(Place::NewRow(editor.rows.len())));
    }

    if let Some(current) = drag {
        let escape = ui.input(|input| input.key_pressed(Pressed::Escape));
        // No handle this frame: the tile is gone or the editor was covered.
        if escape || done.released.unwrap_or(true) {
            if !escape
                && done.released == Some(true)
                && let Drag::Move { key, .. } = current
                && let Some(pointer) = pointer.filter(|&pointer| clip.contains(pointer))
                && let Some(place) = target(&laid, key, pointer)
            {
                done.change = Some(Change::Move(key, place));
            }
            drag = None;
        } else if let Some(pointer) = pointer {
            match current {
                Drag::Move { key, width } => {
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                    if clip.contains(pointer) {
                        if let Some(place) = target(&laid, key, pointer) {
                            landing(ui, &laid, place);
                        }
                        scroll(ui, clip, viewport, pointer);
                    }
                    if let Some((row, at)) = editor.find(key) {
                        let tile = &editor.rows[row].tiles[at].tile;
                        // Beside the pointer, so it doesn't hide where the tile lands.
                        let at = pointer + Vec2::splat(ui::SPACE_M);
                        lifted(ui, reader, art, tile, at, width.min(LIFTED_WIDTH));
                    }
                }
                Drag::Resize { .. } => ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal),
            }
            ui.ctx().request_repaint();
        }
    }
    drag = done.started.or(drag);
    ui.data_mut(|data| match drag {
        Some(drag) => {
            data.insert_temp(drag_id, drag);
        }
        None => {
            data.remove::<Drag>(drag_id);
        }
    });
    if done.closed && done.change.is_none() {
        done.change = Some(Change::Write(None));
    }
    if let Some(at) = done.at {
        editor.block = at;
    }
    if let Some(change) = done.change {
        editor.change(change);
        ui.ctx().request_repaint();
    }
}

/// The floating copy of the tile being moved.
fn lifted(ui: &Ui, reader: Reader, art: &mut Art, tile: &Tile, at: Pos2, width: f32) {
    Area::new(UiId::unique(("page-lifted", reader.channel)))
        .order(Order::Tooltip)
        .fixed_pos(at)
        .constrain(false)
        .interactable(false)
        .show(ui.ctx(), |ui| {
            Frame::popup(ui.style())
                .inner_margin(Margin::same(PAD as i8))
                .show(ui, |ui| {
                    ui.set_width((width - 2.0 * PAD).max(80.0));
                    ui.multiply_opacity(0.85);
                    ui.set_max_height(LIFTED_HEIGHT);
                    let mut clip = ui.clip_rect();
                    clip.max.y = clip.max.y.min(ui.cursor().top() + LIFTED_HEIGHT);
                    ui.set_clip_rect(clip);
                    reader.show(ui, art, tile, false);
                });
        });
}

/// Scroll while a drag is held at the top or bottom edge.
fn scroll(ui: &Ui, clip: Rect, viewport: Rect, pointer: Pos2) {
    let speed = crate::reorder::scroll_speed(clip.bottom() - pointer.y)
        - crate::reorder::scroll_speed(pointer.y - clip.top());
    // From the top of what scrolls, so a banner over the column counts too.
    let top = clip.top() - viewport.top();
    let end = (ui.min_rect().bottom() - top - viewport.height() + 2.0 * ui::SPACE_L).max(0.0);
    let step = speed * ui.input(|input| input.stable_dt.min(0.1));
    let next = (viewport.top() + step).clamp(0.0, end);
    if speed != 0.0 && (next - viewport.top()).abs() > 0.001 {
        ui.scroll_with_delta(vec2(0.0, viewport.top() - next));
    }
}

/// The gap between rows: a click there adds a tile.
fn between(ui: &mut Ui, id: UiId, full: bool) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), BETWEEN), Sense::hover());
    if full {
        return false;
    }
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.tile_add")));
    if response.hovered() || response.has_focus() {
        let color = ui::accent(ui).base;
        let painter = ui.painter();
        painter.hline(rect.x_range(), rect.center().y, Stroke::new(1.0, color));
        painter.circle_filled(rect.center(), 9.0, color);
        let font = FontId::proportional(ui::TEXT_BODY);
        painter.text(rect.center(), Align2::CENTER_CENTER, "+", font, ui::ON_FILL);
    }
    let clicked = response.clicked();
    response.on_hover_cursor(CursorIcon::PointingHand);
    clicked
}

/// The room left beside a row's tiles.
fn empty_slot(ui: &Ui, rect: Rect, id: UiId) -> bool {
    let response = ui.interact(rect, id, Sense::click());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.tile_add")));
    let active = response.hovered() || response.has_focus();
    let c = ui::colors(ui);
    let color = if active { c.strong } else { c.muted };
    let painter = ui.painter();
    painter.rect_stroke(
        rect,
        ui::RADIUS,
        Stroke::new(1.0, c.border),
        StrokeKind::Inside,
    );
    let font = FontId::proportional(ui::TEXT_TITLE);
    painter.text(rect.center(), Align2::CENTER_CENTER, "+", font, color);
    let clicked = response.clicked();
    response.on_hover_cursor(CursorIcon::PointingHand);
    clicked
}

/// A tile's right edge: drag it to resize, snapping to the grid.
#[allow(clippy::too_many_arguments)]
fn edge(
    ui: &Ui,
    grid: &Grid,
    (before, span): (usize, usize),
    tile: Rect,
    id: UiId,
    key: Key,
    drag: &mut Option<Drag>,
    done: &mut Done,
) {
    let grab = Rect::from_min_max(
        pos2(tile.right() - 2.0, tile.top() + STRIP),
        pos2(tile.right() - 2.0 + GRAB, tile.bottom()),
    );
    let response = ui.interact(grab, id.with("edge"), Sense::drag());
    response.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.tile_resize")));
    if response.drag_started() {
        *drag = Some(Drag::Resize { key });
    }
    let resizing = matches!(*drag, Some(Drag::Resize { key: held }) if held == key);
    if resizing {
        done.held(&response);
        if let Some(pointer) = response.interact_pointer_pos() {
            let end = (pointer.x - grid.left + GAP / 2.0) / grid.unit;
            let wanted = (end.round().max(0.0) as usize).saturating_sub(before);
            if wanted.max(MIN_WIDTH) != span {
                done.change = Some(Change::Edge(key, wanted));
            }
        }
        // The grid it snaps to, and how wide it is now.
        let painter = ui.painter();
        let line = Stroke::new(1.0, ui::colors(ui).border);
        for twelfth in 1..GRID {
            let x = grid.left + twelfth as f32 * grid.unit - GAP / 2.0;
            painter.vline(x, tile.top()..=tile.top() + 8.0, line);
            painter.vline(x, tile.bottom() - 8.0..=tile.bottom(), line);
        }
        let label = format!("{}%", span * 100 / GRID);
        let font = FontId::new(ui::TEXT_SMALL, emoji::family(true));
        let badge = Rect::from_center_size(tile.center(), vec2(52.0, 26.0));
        painter.rect_filled(badge, ui::RADIUS, ui::accent(ui).base);
        painter.text(
            badge.center(),
            Align2::CENTER_CENTER,
            label,
            font,
            ui::ON_FILL,
        );
    }
    let active = resizing || response.hovered();
    let color = match active {
        true => ui::accent(ui).base,
        false => ui::colors(ui).border,
    };
    let bar = Rect::from_center_size(
        pos2(tile.right(), grab.center().y),
        vec2(4.0, grab.height().clamp(0.0, 36.0)),
    );
    ui.painter().rect_filled(bar, 2.0, color);
    response.on_hover_cursor(CursorIcon::ResizeHorizontal);
}

/// One tile of the editor.
struct Shown<'a> {
    reader: Reader<'a>,
    writing: bool,
    focus: bool,
    lifted: bool,
    /// The space between a text's lines, as readers get it.
    lines: f32,
    /// The block being written, and where its cursor goes with `focus`.
    active: usize,
    caret: Option<usize>,
    moves: &'a Moves,
    id: UiId,
}

impl Shown<'_> {
    fn show(&self, ui: &mut Ui, art: &mut Art, editing: &mut Editing, done: &mut Done) {
        let key = editing.key;
        let origin = ui.max_rect().min;
        let width = ui.available_width();
        ui.spacing_mut().item_spacing.y = self.lines;
        if self.lifted {
            ui.multiply_opacity(0.3);
        }
        padded(ui, STRIP, |ui| match &mut editing.tile {
            Tile::Blocks(blocks) if self.writing => {
                let single = blocks.len() == 1;
                for (index, block) in blocks.iter_mut().enumerate() {
                    let part = Field {
                        key,
                        shape: editing.shape,
                        block: index,
                    };
                    match block {
                        Block::Markdown(text) => self.write(ui, part, single, text, done),
                        Block::Picture(picture) => {
                            let id = field(self.reader.channel, part);
                            if let Some(change) = pictures::edit_picture(ui, art, picture, id) {
                                done.change = Some(Change::Picture(key, index, change));
                            }
                        }
                    }
                }
            }
            Tile::Blocks(blocks) if blank(blocks) => {
                ui.weak(t!("home.tile_hint"));
            }
            tile => self.reader.show(ui, art, tile, false),
        });
        let rect = Rect::from_min_size(origin, vec2(width, ui.min_rect().height()));
        let c = ui::colors(ui);
        let pointed = ui.rect_contains_pointer(rect);
        let mut outline = pointed.then_some(c.border);

        let grip_rect =
            Rect::from_min_size(pos2(rect.center().x - 24.0, rect.top()), vec2(48.0, STRIP));
        let start = |response: &eframe::egui::Response, done: &mut Done| {
            if response.drag_started() {
                done.started = Some(Drag::Move { key, width });
            }
            if self.lifted {
                done.held(response);
            }
        };
        if !self.writing {
            // Over the content, so links and code buttons don't take the click: the
            // whole tile moves, and a click writes in it.
            let body = ui.interact(rect, self.id.with("body"), Sense::click_and_drag());
            body.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.tile_write")));
            start(&body, done);
            if body.clicked() && matches!(editing.tile, Tile::Blocks(_)) {
                done.change = Some(Change::Write(Some(key)));
            }
            if body.has_focus() {
                outline = Some(ui::accent(ui).base);
            }
            body.on_hover_cursor(CursorIcon::Grab);
        } else {
            outline = Some(ui::accent(ui).base);
        }
        if let Some(color) = outline {
            ui.painter().rect_stroke(
                rect,
                ui::RADIUS,
                Stroke::new(1.0, color),
                StrokeKind::Inside,
            );
        }

        let id = UiId::unique(("page-grip", self.reader.channel, key));
        let grip = ui.interact(grip_rect, id, Sense::drag());
        grip.widget_info(|| WidgetInfo::labeled(Role::Button, true, t!("home.tile_drag")));
        start(&grip, done);
        let color: Color32 = if pointed || self.writing {
            c.strong
        } else {
            c.muted
        };
        let font = FontId::proportional(ui::TEXT_ICON);
        ui.painter()
            .text(grip_rect.center(), Align2::CENTER_CENTER, GRIP, font, color);
        grip.on_hover_text(t!("home.tile_drag"))
            .on_hover_cursor(CursorIcon::Grab);

        self.menu(ui, rect, editing, done);
    }

    fn write(&self, ui: &mut Ui, part: Field, single: bool, text: &mut String, done: &mut Done) {
        let id = field(self.reader.channel, part);
        let hint = if single { t!("home.tile_hint") } else { "" };
        markup::shortcuts(ui, id, text);
        let had_focus = ui.memory(|memory| memory.has_focus(id));
        let mut layouter =
            |ui: &Ui, text: &dyn TextBuffer, width| markup::layout(ui, text.as_str(), width);
        let response = ui.add(
            TextEdit::multiline(text)
                .id(id)
                .layouter(&mut layouter)
                .frame(Frame::NONE)
                .font(emoji::font(ui::TEXT_BODY))
                .hint_text(hint)
                .desired_rows(1)
                .desired_width(f32::INFINITY),
        );
        if response.has_focus() {
            done.at = Some(part.block);
        }
        if self.focus && part.block == self.active {
            response.request_focus();
            // Where asked, else where the text ends.
            let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
            if self.caret.is_some() || state.cursor.char_range().is_none() {
                let at = self.caret.unwrap_or_else(|| text.chars().count());
                let end = eframe::egui::text::CCursor::new(at);
                state
                    .cursor
                    .set_char_range(Some(eframe::egui::text::CCursorRange::one(end)));
                state.store(ui.ctx(), id);
            }
        }
        let (enter, escape) = ui.input(|input| {
            (
                input.key_pressed(Pressed::Enter) && !input.modifiers.shift,
                input.key_pressed(Pressed::Escape),
            )
        });
        if had_focus && enter && response.changed() {
            markup::entered(ui, id, text);
        }
        // egui takes the focus away on Escape before the field sees it.
        if escape && (had_focus || response.lost_focus()) {
            done.closed = true;
        }
    }

    fn menu(&self, ui: &mut Ui, rect: Rect, editing: &Editing, done: &mut Done) {
        let key = editing.key;
        let spot = Rect::from_min_size(
            pos2(rect.right() - STRIP - 8.0, rect.top()),
            Vec2::splat(STRIP),
        );
        let mut corner = ui.new_child(
            UiBuilder::new()
                .scope_id(self.id.with("options"))
                .max_rect(spot),
        );
        let more = ui::icon_button(
            &mut corner,
            MORE,
            t!("home.tile_menu"),
            ui::Look::Ghost,
            spot.size(),
        );
        ui::menu(&more, |menu| {
            let mut pick = None;
            let width = editing.width;
            let row_mates =
                self.moves.steps.iter().any(|(step, place)| {
                    matches!(step, Step::Left | Step::Right) && place.is_some()
                });
            for (step, place) in self.moves.steps {
                let (icon, text) = match step {
                    Step::Up => ("⬆", t!("home.tile_up")),
                    Step::Down => ("⬇", t!("home.tile_down")),
                    Step::Left => ("⬅", t!("home.tile_left")),
                    Step::Right => ("➡", t!("home.tile_right")),
                };
                let item = ui::Item::new(icon, text).enabled(place.is_some());
                if menu.add(item).clicked()
                    && let Some(place) = place
                {
                    pick = Some(Change::Move(key, place));
                }
            }
            menu.separator();
            let wider = ui::Item::new("↔", t!("home.tile_wider")).enabled(width < GRID);
            if menu.add(wider).clicked() {
                pick = Some(Change::Width(key, width + 1));
            }
            let narrower = ui::Item::new("↔", t!("home.tile_narrower")).enabled(width > MIN_WIDTH);
            if menu.add(narrower).clicked() {
                pick = Some(Change::Width(key, width - 1));
            }
            let whole =
                ui::Item::new("↔", t!("home.tile_whole")).enabled(width < GRID && !row_mates);
            if menu.add(whole).clicked() {
                pick = Some(Change::Width(key, GRID));
            }
            let beside =
                ui::Item::new("+", t!("home.tile_beside")).enabled(self.moves.beside.is_some());
            if menu.add(beside).clicked()
                && let Some(place) = self.moves.beside
            {
                pick = Some(Change::Add(place));
            }
            menu.separator();
            if menu.danger("🗑", t!("home.tile_remove")).clicked() {
                pick = Some(Change::Remove(key));
            }
            if pick.is_some() {
                done.change = pick;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Event, Modifiers, PointerButton};
    use rusttalk_core::pages::{Placed, Row};

    use super::*;

    fn rect(left: f32, right: f32, top: f32, bottom: f32) -> Rect {
        Rect::from_min_max(pos2(left, top), pos2(right, bottom))
    }

    /// A row of two that takes no more, over one that does.
    fn rows() -> Vec<Laid> {
        vec![
            Laid {
                rect: rect(0., 600., 100., 300.),
                tiles: vec![
                    (1, rect(0., 290., 100., 300.)),
                    (2, rect(310., 600., 100., 250.)),
                ],
                takes_more: false,
                stacked: false,
            },
            Laid {
                rect: rect(0., 600., 350., 550.),
                tiles: vec![(3, rect(0., 290., 350., 550.))],
                takes_more: true,
                stacked: false,
            },
        ]
    }

    #[test]
    fn drags_point_at_rows_gaps_and_tiles() {
        let rows = rows();
        for (dragged, x, y, expected) in [
            (3, 100., 50., Some(Place::NewRow(0))),
            (3, 100., 105., Some(Place::NewRow(0))),
            (3, 100., 295., Some(Place::NewRow(1))),
            (3, 100., 320., Some(Place::NewRow(1))),
            (1, 100., 700., Some(Place::NewRow(2))),
            (3, 400., 200., Some(Place::Swap(2))),
            (3, 300., 200., None),
            (1, 500., 200., Some(Place::Into(0, 2))),
            (1, 100., 200., None),
            (1, 100., 450., Some(Place::Into(1, 0))),
            (1, 450., 450., Some(Place::Into(1, 1))),
            (1, 900., 450., None),
        ] {
            assert_eq!(
                target(&rows, dragged, pos2(x, y)),
                expected,
                "{dragged} {x} {y}"
            );
        }
        assert_eq!(target(&[], 1, pos2(1., 1.)), None);
    }

    #[test]
    fn narrow_tiles_stack_their_row() {
        assert!(!Grid::new(0., 700., &[4, 4, 4]).stacked);
        assert!(Grid::new(0., 420., &[4, 4, 4]).stacked);
        assert!(!Grid::new(0., 420., &[6, 6]).stacked);
        assert!(!Grid::new(0., 100., &[12]).stacked);
    }

    /// The editor through real frames and input, like a person at the mouse.
    struct Screen {
        ctx: eframe::egui::Context,
        files: crate::files::Files,
        editor: Editor,
        dir: Directory,
        time: f64,
        width: f32,
    }

    impl Screen {
        fn new(width: f32, layout: &Layout) -> Self {
            let ctx = eframe::egui::Context::default();
            ui::install(&ctx);
            let mut screen = Self {
                ctx,
                files: crate::files::Files::new(std::path::Path::new("no-such-profile")),
                editor: Editor::new(1, layout),
                dir: Directory::default(),
                time: 0.,
                width,
            };
            screen.frame(Vec::new());
            screen.frame(Vec::new());
            screen
        }

        fn frame(&mut self, events: Vec<Event>) {
            self.time += 0.2;
            let input = eframe::egui::RawInput {
                screen_rect: Some(rect(0., self.width, 0., 800.)),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let (editor, dir, files) = (&mut self.editor, &self.dir, &mut self.files);
            let address = url::Url::parse("https://friends.example.org/").unwrap();
            let (commands, _asked) = tokio::sync::mpsc::unbounded_channel();
            let mut art = Art {
                files,
                address: &address,
                commands: &commands,
                local: Vec::new(),
            };
            let mut output = self.ctx.run_ui(input, |ui| {
                eframe::egui::ScrollArea::vertical()
                    .auto_shrink(false)
                    .show_viewport(ui, |ui, viewport| {
                        surface(ui);
                        let reader = Reader {
                            dir,
                            channel: [7; 32],
                            person: [9; 32],
                        };
                        column(ui, |ui| edit(ui, reader, &mut art, editor, viewport));
                    });
            });
            output.textures_delta.clear();
        }

        fn key(&self, row: usize, at: usize) -> Key {
            self.editor.rows[row].tiles[at].key
        }

        fn part(&self, key: Key, part: &str) -> Rect {
            let tile = UiId::unique(("page-tile", [7_u8; 32], key));
            let id = match part {
                "grip" => UiId::unique(("page-grip", [7_u8; 32], key)),
                _ => tile.with(part),
            };
            self.ctx.read_response(id).expect("not on screen").rect
        }

        fn button(pos: Pos2, pressed: bool) -> Event {
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            }
        }

        fn click(&mut self, at: Pos2) {
            self.frame(vec![Event::PointerMoved(at)]);
            self.frame(vec![Self::button(at, true)]);
            // Within the time egui still counts as a click.
            self.time -= 0.15;
            self.frame(vec![Self::button(at, false)]);
            self.frame(Vec::new());
            self.frame(Vec::new());
        }

        fn press(&mut self, key: Pressed, modifiers: Modifiers) {
            for pressed in [true, false] {
                self.frame(vec![Event::Key {
                    key,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers,
                }]);
            }
            self.frame(Vec::new());
        }

        fn write(&mut self, text: &str) {
            self.frame(vec![Event::Text(text.into())]);
            self.frame(Vec::new());
        }

        fn drag(&mut self, from: Pos2, to: Pos2, escape: bool) {
            self.frame(vec![Event::PointerMoved(from)]);
            self.frame(vec![Self::button(from, true)]);
            self.frame(vec![Event::PointerMoved(from + vec2(12., 12.))]);
            self.frame(vec![Event::PointerMoved(to)]);
            // Holding still must not drop it.
            self.frame(Vec::new());
            if escape {
                self.press(Pressed::Escape, Modifiers::NONE);
            }
            self.frame(vec![Self::button(to, false)]);
            self.frame(Vec::new());
            self.frame(Vec::new());
        }

        fn shown(&self) -> String {
            (self.editor.layout().rows.iter())
                .map(|row| {
                    (row.tiles.iter())
                        .map(|placed| match &placed.tile {
                            Tile::Blocks(blocks) => match blocks.as_slice() {
                                [Block::Markdown(text)] => format!("{text}{}", placed.width),
                                _ => "mixed".into(),
                            },
                            Tile::Unknown { .. } => "?".into(),
                        })
                        .collect::<Vec<_>>()
                        .join("+")
                })
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    fn page(rows: &[&[(&str, usize)]]) -> Layout {
        Layout {
            banner: None,
            title: "Title".into(),
            rows: (rows.iter())
                .map(|tiles| Row {
                    tiles: (tiles.iter())
                        .map(|&(text, width)| Placed {
                            width,
                            tile: Tile::markdown(text),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn the_whole_tile_drags_to_move() {
        let layout = page(&[&[("a", 6), ("b", 6)], &[("c", 6)]]);
        let mut screen = Screen::new(800., &layout);
        // By its content, not the handle, to the right of c.
        let a = screen.part(screen.key(0, 0), "body");
        let c = screen.part(screen.key(1, 0), "body");
        let to = pos2(c.right() + 60., c.center().y);
        screen.drag(a.center(), to, false);
        assert_eq!(screen.shown(), "b6 c6+a6");

        // Escape puts it back.
        let mut screen = Screen::new(800., &layout);
        screen.drag(a.center(), to, true);
        assert_eq!(screen.shown(), "a6+b6 c6");

        // The handle works too. Below everything is a row of its own.
        let mut screen = Screen::new(800., &layout);
        let grip = screen.part(screen.key(0, 0), "grip");
        screen.drag(grip.center(), pos2(300., 760.), false);
        assert_eq!(screen.shown(), "b6 c6 a12");
    }

    #[test]
    fn dragging_the_edge_changes_the_width() {
        let mut screen = Screen::new(800., &page(&[&[("a", 6), ("b", 6)]]));
        let a = screen.key(0, 0);
        let edge = screen.part(a, "edge").center();
        screen.drag(edge, edge + vec2(130., 0.), false);
        assert_eq!(screen.shown(), "a8+b4");

        let mut screen = Screen::new(800., &page(&[&[("a", 4)]]));
        let edge = screen.part(screen.key(0, 0), "edge").center();
        screen.drag(edge, pos2(795., edge.y), false);
        assert_eq!(screen.shown(), "a12");
    }

    #[test]
    fn a_click_writes_and_the_shortcuts_format() {
        let mut screen = Screen::new(800., &page(&[&[("hi", 12)]]));
        screen.click(screen.part(screen.key(0, 0), "body").center());
        screen.write(" you");
        assert_eq!(screen.shown(), "hi you12");
        screen.press(Pressed::A, Modifiers::COMMAND);
        screen.press(Pressed::B, Modifiers::COMMAND);
        assert_eq!(screen.shown(), "**hi you**12");
        screen.press(Pressed::Escape, Modifiers::NONE);
        assert!(screen.editor.writing.is_none());
    }

    #[test]
    fn a_new_page_types_its_title_then_its_first_tile() {
        let mut screen = Screen::new(800., &Layout::default());
        screen.write("Game night");
        screen.press(Pressed::Enter, Modifiers::NONE);
        screen.write("- bring snacks");
        screen.press(Pressed::Enter, Modifiers::NONE);
        screen.write("dice");
        assert_eq!(screen.editor.layout().title, "Game night");
        assert_eq!(screen.shown(), "- bring snacks\n- dice12");
    }

    #[test]
    fn a_narrow_window_stacks_tiles_in_reading_order() {
        let layout = page(&[&[("a", 4), ("b", 4), ("c", 4)]]);
        let at = |width| {
            let screen = Screen::new(width, &layout);
            [0, 1, 2].map(|at| screen.part(screen.key(0, at), "body").center())
        };
        let [a, b, c] = at(800.);
        assert!(a.y == b.y && b.y == c.y && a.x < b.x && b.x < c.x);
        let [a, b, c] = at(360.);
        assert!(a.x == b.x && b.x == c.x && a.y < b.y && b.y < c.y);
    }

    #[test]
    fn the_menu_moves_a_tile_with_the_keyboard_alone() {
        let mut screen = Screen::new(800., &page(&[&[("a", 12)], &[("b", 12)]]));
        let focused = |screen: &Screen| {
            let id = screen.ctx.memory(|memory| memory.focused())?;
            Some(screen.ctx.read_response(id)?.rect)
        };
        let top = screen.part(screen.key(0, 0), "grip").center().y;
        let found = (0..30).any(|_| {
            screen.press(Pressed::Tab, Modifiers::NONE);
            focused(&screen)
                .is_some_and(|rect| (rect.center().y - top).abs() < 2. && rect.width() <= STRIP)
        });
        assert!(found, "the options button takes no focus");
        screen.press(Pressed::Enter, Modifiers::NONE);
        // The first item that can be chosen is Move down.
        let found = (0..30).any(|_| {
            screen.press(Pressed::Tab, Modifiers::NONE);
            focused(&screen).is_some_and(|rect| rect.height() == ui::MENU_ROW)
        });
        assert!(found, "the menu takes no focus");
        screen.press(Pressed::Enter, Modifiers::NONE);
        assert_eq!(screen.shown(), "b12 a12");
    }

    #[test]
    fn the_editors_icons_are_in_the_bundled_fonts() {
        let screen = Screen::new(800., &page(&[&[("a", 12)]]));
        let font = FontId::proportional(ui::TEXT_ICON);
        for icon in [
            GRIP, MORE, "+", "↔", "🗑", "⏶", "⏷", "⬆", "⬇", "⬅", "➡", "•", "“", "🔗", "{ }",
        ] {
            let there = screen.ctx.fonts_mut(|fonts| fonts.has_glyphs(&font, icon));
            assert!(there, "{icon} would show as a box");
        }
    }
}
