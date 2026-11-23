//! Dragging channels and categories into a new order, for people who may manage channels.
//!
//! Rows are laid out as usual, then drawn shifted by an animated offset, so the other
//! rows slide aside to show where the dragged one would land. The dragged row follows
//! the pointer as a floating copy while its own place stays empty.

use std::collections::{HashMap, HashSet};

use eframe::egui::{CursorIcon, Id as UiId, Key, Rect, Response, Sense, Ui, UiBuilder, vec2};

use crate::{
    server::{Category, Id},
    ui::{self, Item as MenuItem, SLIDE_SECONDS},
};

const SCROLL_EDGE: f32 = 40.0;
const SCROLL_SPEED: f32 = 600.0;

fn scroll_speed(distance: f32) -> f32 {
    (1.0 - distance / SCROLL_EDGE).clamp(0.0, 1.0) * SCROLL_SPEED
}

/// A row in the channel list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    Category(Id),
    Channel(Id),
}

/// Where an item goes, counted without the item itself: a category's place,
/// or a category and a place among its channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Category(usize),
    Channel(usize, usize),
}

/// The order to save: category IDs, each with its channel IDs. `None` when a
/// channel has no category, which cannot be saved.
pub fn order(categories: &[Category]) -> Option<Vec<(Id, Vec<Id>)>> {
    (categories.iter())
        .map(|c| Some((c.id?, c.channels.iter().map(|ch| ch.id).collect())))
        .collect()
}

fn find(categories: &[Category], channel: Id) -> Option<(usize, usize)> {
    categories.iter().enumerate().find_map(|(c, category)| {
        let at = category.channels.iter().position(|ch| ch.id == channel)?;
        Some((c, at))
    })
}

/// Move an item to a slot. Returns whether the order changed.
pub fn place(categories: &mut Vec<Category>, item: Item, slot: Slot) -> bool {
    let before = order(categories);
    match (item, slot) {
        (Item::Category(id), Slot::Category(to)) => {
            let Some(from) = categories.iter().position(|c| c.id == Some(id)) else {
                return false;
            };
            let moved = categories.remove(from);
            categories.insert(to.min(categories.len()), moved);
        }
        (Item::Channel(id), Slot::Channel(to, at)) if to < categories.len() => {
            let Some((from, index)) = find(categories, id) else {
                return false;
            };
            let moved = categories[from].channels.remove(index);
            let list = &mut categories[to].channels;
            list.insert(at.min(list.len()), moved);
        }
        _ => return false,
    }
    order(categories) != before
}

/// The slot one step up or down, for "Move up" and "Move down". A channel at
/// the edge of its category moves into the next category. `None` at either end.
pub fn step(categories: &[Category], item: Item, up: bool) -> Option<Slot> {
    match item {
        Item::Category(id) => {
            let at = categories.iter().position(|c| c.id == Some(id))?;
            let to = if up { at.checked_sub(1)? } else { at + 1 };
            (to < categories.len()).then_some(Slot::Category(to))
        }
        Item::Channel(id) => {
            let (c, at) = find(categories, id)?;
            if up {
                if at > 0 {
                    return Some(Slot::Channel(c, at - 1));
                }
                let previous = c.checked_sub(1)?;
                Some(Slot::Channel(previous, categories[previous].channels.len()))
            } else if at + 1 < categories[c].channels.len() {
                Some(Slot::Channel(c, at + 1))
            } else {
                (c + 1 < categories.len()).then_some(Slot::Channel(c + 1, 0))
            }
        }
    }
}

/// "Move up" and "Move down" menu items, dimmed at either end, and for a channel
/// t!("reorder.move_to") with the other categories; returns the chosen move.
pub fn menu(m: &mut ui::Menu, categories: &[Category], item: Item) -> Option<(Item, Slot)> {
    let mut chosen = None;
    for (icon, label, up) in [
        ("⬆", t!("reorder.up"), true),
        ("⬇", t!("reorder.down"), false),
    ] {
        let slot = step(categories, item, up);
        if m.add(MenuItem::new(icon, label).enabled(slot.is_some()))
            .clicked()
        {
            chosen = slot.map(|slot| (item, slot));
        }
    }
    if let Item::Channel(id) = item {
        let here = find(categories, id).map(|(c, _)| c);
        let others: Vec<_> = (categories.iter().enumerate())
            .filter(|(at, c)| Some(*at) != here && c.id.is_some())
            .collect();
        if !others.is_empty() {
            let picked = m.submenu("↪", t!("reorder.move_to"), |m| {
                let mut picked = None;
                for (at, category) in others {
                    let name = if category.name.is_empty() {
                        t!("reorder.no_category")
                    } else {
                        &category.name
                    };
                    if m.item("🗀", name).clicked() {
                        picked = Some(Slot::Channel(at, category.channels.len()));
                    }
                }
                picked
            });
            if let Some(slot) = picked.flatten() {
                chosen = Some((item, slot));
            }
        }
    }
    chosen
}

/// Where the dragged item would land with its floating copy's top at `top`, given
/// each row's last layout (height including spacing), and how far each row moves aside.
#[derive(Debug, PartialEq)]
pub struct Target {
    pub slot: Slot,
    pub offsets: HashMap<Item, f32>,
    /// The dragged rows, drawn as an empty gap.
    pub hidden: Vec<Item>,
}

pub fn target(
    categories: &[Category],
    rects: &HashMap<Item, Rect>,
    dragged: Item,
    top: f32,
    scroll_delta: f32,
) -> Option<Target> {
    // Compare the floating row with the last frame's layout in the same coordinates.
    let top = top + scroll_delta;
    // What moves as one: each row when dragging a channel, a whole category otherwise.
    let mut units: Vec<Vec<Item>> = Vec::new();
    for category in categories {
        let rows = (category.id.map(Item::Category).into_iter())
            .chain(category.channels.iter().map(|ch| Item::Channel(ch.id)));
        match dragged {
            Item::Category(_) => units.push(rows.collect()),
            Item::Channel(_) => units.extend(rows.map(|row| vec![row])),
        }
    }
    let from = units
        .iter()
        .position(|unit| unit.first() == Some(&dragged))?;
    let spans: Vec<(f32, f32)> = (units.iter())
        .map(|unit| {
            let top = rects.get(unit.first()?)?.top();
            let height: Option<f32> = unit.iter().map(|i| rects.get(i).map(Rect::height)).sum();
            Some((top, height?))
        })
        .collect::<Option<_>>()?;
    let gap = spans[from].1;
    // Past a row once the copy's top passes that row's middle in the list without
    // the dragged unit: the same as comparing middles, whatever the sizes.
    let rest: Vec<usize> = (0..units.len()).filter(|i| *i != from).collect();
    let insert = (rest.iter())
        .filter(|&&i| {
            let (start, height) = spans[i];
            let start = if i > from { start - gap } else { start };
            start + height / 2.0 < top
        })
        .count();
    let mut offsets = HashMap::new();
    for (index, &i) in rest.iter().enumerate() {
        let shift = match (index >= insert, i > from) {
            (true, false) => gap,
            (false, true) => -gap,
            _ => 0.0,
        };
        for item in &units[i] {
            offsets.insert(*item, shift);
        }
    }
    let slot = match dragged {
        Item::Category(_) => Slot::Category(insert),
        // The row just above the gap decides the category and place.
        Item::Channel(_) => match insert.checked_sub(1).map(|index| units[rest[index]][0]) {
            None => Slot::Channel(0, 0),
            Some(Item::Category(id)) => {
                Slot::Channel(categories.iter().position(|c| c.id == Some(id))?, 0)
            }
            Some(Item::Channel(id)) => {
                let (c, at) = find(categories, id)?;
                let earlier =
                    (categories[c].channels[..at].iter()).any(|ch| Item::Channel(ch.id) == dragged);
                Slot::Channel(c, at + 1 - usize::from(earlier))
            }
        },
    };
    Some(Target {
        slot,
        offsets,
        hidden: units[from].clone(),
    })
}

/// The full order to save: `shown` as this person sees it, with what they
/// can't see (empty categories, channels they can't read) kept in place.
/// `categories` and `channels` (with their category) are the server's full lists, in order.
pub fn merge(
    shown: &[(Id, Vec<Id>)],
    categories: &[Id],
    channels: &[(Id, Id)],
) -> Vec<(Id, Vec<Id>)> {
    let shown_categories: Vec<Id> = shown.iter().map(|(id, _)| *id).collect();
    let shown_channels: HashSet<Id> = shown.iter().flat_map(|(_, list)| list.clone()).collect();
    fill(categories, &shown_categories, |id| {
        shown_categories.contains(id)
    })
    .into_iter()
    .map(|category| {
        let current: Vec<Id> = (channels.iter())
            .filter(|(_, c)| *c == category)
            .map(|(id, _)| *id)
            .collect();
        let wanted = (shown.iter())
            .find(|(id, _)| *id == category)
            .map(|(_, list)| list.clone())
            .unwrap_or_default();
        let list = fill(&current, &wanted, |id| shown_channels.contains(id));
        (category, list)
    })
    .collect()
}

/// `current` with each shown item's place taken by the next wanted one, leftovers last.
fn fill(current: &[Id], wanted: &[Id], shown: impl Fn(&Id) -> bool) -> Vec<Id> {
    let mut next = wanted.iter().copied();
    let mut list: Vec<Id> = (current.iter())
        .filter_map(|id| if shown(id) { next.next() } else { Some(*id) })
        .collect();
    list.extend(next);
    list
}

#[derive(Clone)]
struct Drag {
    item: Item,
    /// Pointer position within the grabbed row.
    grab: eframe::egui::Vec2,
    width: f32,
    label: String,
}

#[derive(Clone, Copy)]
struct Landing {
    item: Item,
    /// Where the floating copy was let go.
    top: f32,
    /// How far the rows of a landing category start from their places.
    delta: Option<f32>,
}

#[derive(Clone, Default)]
struct State {
    drag: Option<Drag>,
    /// Each row's place in the last frame, height including spacing.
    rects: HashMap<Item, Rect>,
    origin_y: f32,
    landing: Option<Landing>,
    /// The order just changed: rows are already in their new places.
    snap: bool,
}

/// One frame of the channel list. Call `row` for each row, then `end`.
pub struct List {
    id: UiId,
    allowed: bool,
    state: State,
    landing: Option<Landing>,
    snap: bool,
    target: Option<Target>,
    handles: Vec<(Item, Response)>,
    clip: Rect,
}

impl List {
    /// `allowed`: this person may reorder now. Otherwise rows show as usual, without drag.
    pub fn begin(ui: &Ui, id: UiId, categories: &[Category], allowed: bool) -> Self {
        let mut state: State = ui.data(|data| data.get_temp(id)).unwrap_or_default();
        if !allowed {
            state.drag = None;
        }
        let pointer = ui.ctx().pointer_interact_pos();
        let origin_y = ui.cursor().min.y;
        let target = state
            .drag
            .as_ref()
            .zip(pointer)
            .and_then(|(drag, pointer)| {
                target(
                    categories,
                    &state.rects,
                    drag.item,
                    pointer.y - drag.grab.y,
                    state.origin_y - origin_y,
                )
            });
        state.rects.clear();
        state.origin_y = origin_y;
        Self {
            id,
            allowed,
            landing: state.landing.take(),
            snap: std::mem::take(&mut state.snap),
            state,
            target,
            handles: Vec::new(),
            clip: ui.clip_rect(),
        }
    }

    /// Lay out one row of category `block`; `add` draws it and returns the part to grab.
    pub fn row(
        &mut self,
        ui: &mut Ui,
        item: Item,
        block: Id,
        add: impl FnOnce(&mut Ui) -> Response,
    ) -> Response {
        let top = ui.cursor().min;
        let ctx = ui.ctx().clone();
        let slide = self.id.with(item);
        if self.snap {
            ctx.animate_value_with_time(slide, 0.0, 0.0);
        }
        if let Some(landing) = &mut self.landing {
            let delta = if landing.item == item {
                Some(landing.top - top.y)
            } else if landing.item == Item::Category(block) {
                landing.delta
            } else {
                None
            };
            if let Some(delta) = delta {
                landing.delta = Some(delta);
                ctx.animate_value_with_time(slide, delta, 0.0);
            }
        }
        let shift = (self.target.as_ref())
            .and_then(|target| target.offsets.get(&item).copied())
            .unwrap_or(0.0);
        let dy = ctx.animate_value_with_time(slide, shift, SLIDE_SECONDS);
        let rect = ui.available_rect_before_wrap().translate(vec2(0.0, dy));
        let mut child = ui.new_child(UiBuilder::new().id_salt(item).max_rect(rect));
        if (self.target.as_ref()).is_some_and(|target| target.hidden.contains(&item)) {
            // The dragged row's own place shows as empty space: the drop slot.
            child.multiply_opacity(0.0);
        }
        let handle = add(&mut child);
        let laid = Rect::from_min_size(top, child.min_rect().size());
        ui.advance_cursor_after_rect(laid);
        let spacing = ui.spacing().item_spacing.y;
        self.state
            .rects
            .insert(item, laid.with_max_y(laid.max.y + spacing));
        if !self.allowed {
            return handle;
        }
        let handle = handle
            .interact(Sense::click_and_drag())
            .on_hover_cursor(match item {
                Item::Channel(_) => CursorIcon::PointingHand,
                Item::Category(_) => CursorIcon::Grab,
            });
        self.handles.push((item, handle.clone()));
        handle
    }

    /// Finish the frame; returns a drop that should be applied with `place`.
    pub fn end(mut self, ui: &Ui, categories: &[Category], viewport: Rect) -> Option<(Item, Slot)> {
        let pointer = ui.ctx().pointer_interact_pos();
        let mut dropped = None;
        if let Some(drag) = self.state.drag.take() {
            let handle = (self.handles.iter()).find(|(item, _)| *item == drag.item);
            let stopped = handle.is_none_or(|(_, handle)| handle.drag_stopped());
            let escape = ui.input(|input| input.key_pressed(Key::Escape));
            match pointer {
                Some(pointer) if !stopped && !escape => {
                    ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                    let at = pointer - drag.grab;
                    ui::drag_preview(
                        ui.ctx(),
                        self.id.with("floating"),
                        at,
                        drag.width,
                        &drag.label,
                    );
                    self.state.drag = Some(drag);
                }
                _ => {
                    // Escape, a drop outside, or a lost row: everything slides back.
                    let top = pointer.map_or(0.0, |pointer| pointer.y - drag.grab.y);
                    self.state.landing = pointer.map(|_| Landing {
                        item: drag.item,
                        top,
                        delta: None,
                    });
                    let inside = pointer.is_some_and(|pointer| self.clip.contains(pointer));
                    if stopped
                        && !escape
                        && inside
                        && let Some(target) = &self.target
                    {
                        dropped = Some((drag.item, target.slot));
                        self.state.snap = true;
                    }
                }
            }
        } else if let Some(((item, handle), origin)) = (self.handles.iter())
            .find(|(_, handle)| handle.drag_started())
            .zip(ui.input(|input| input.pointer.press_origin()))
        {
            self.state.drag = Some(Drag {
                item: *item,
                grab: origin - handle.rect.min,
                width: handle.rect.width(),
                label: label(categories, *item),
            });
        }
        if self.state.drag.is_some()
            && let Some(pointer) = pointer.filter(|&pointer| self.clip.contains(pointer))
        {
            let speed = scroll_speed(self.clip.bottom() - pointer.y)
                - scroll_speed(pointer.y - self.clip.top());
            let dt = ui.input(|input| input.stable_dt.min(0.1));
            let offset = viewport.top();
            let content_height = ui.min_size().y + ui.spacing().scroll.content_margin.sum().y;
            let max_offset = (content_height - viewport.height()).max(0.0);
            let next = (offset + speed * dt).clamp(0.0, max_offset);
            if speed != 0.0 && (next - offset).abs() > 0.001 {
                // ScrollArea applies this after layout and repaints for the new offset.
                ui.scroll_with_delta(vec2(0.0, offset - next));
            }
        }
        ui.data_mut(|data| data.insert_temp(self.id, self.state));
        dropped
    }
}

fn label(categories: &[Category], item: Item) -> String {
    for category in categories {
        match item {
            Item::Category(id) if category.id == Some(id) => return category.name.to_uppercase(),
            Item::Channel(id) => {
                if let Some(ch) = category.channels.iter().find(|ch| ch.id == id) {
                    let icon = if ch.voice { "🔊" } else { "#" };
                    return format!("{icon}  {}", ch.name);
                }
            }
            Item::Category(_) => {}
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::Channel;

    fn category(id: u8, channels: &[u8]) -> Category {
        Category {
            id: Some([id; 32]),
            name: format!("c{id}"),
            channels: (channels.iter())
                .map(|&ch| Channel {
                    topic: String::new(),
                    id: [ch; 32],
                    name: format!("{ch}"),
                    voice: false,
                    page: false,
                    forum: false,
                    audio_kbps: 64,
                    member_limit: 0,
                    focus_until: None,
                    flags: Vec::new(),
                })
                .collect(),
        }
    }

    fn ids(categories: &[Category]) -> Vec<(u8, Vec<u8>)> {
        order(categories)
            .unwrap()
            .into_iter()
            .map(|(c, list)| (c[0], list.iter().map(|ch| ch[0]).collect()))
            .collect()
    }

    /// Categories 1 [11, 12] and 2 [21], each row 20 high from the top.
    fn sample() -> (Vec<Category>, HashMap<Item, Rect>) {
        let categories = vec![category(1, &[11, 12]), category(2, &[21])];
        let rows = [
            Item::Category([1; 32]),
            Item::Channel([11; 32]),
            Item::Channel([12; 32]),
            Item::Category([2; 32]),
            Item::Channel([21; 32]),
        ];
        let rects = (rows.iter().zip(0..))
            .map(|(item, i)| {
                let top = i as f32 * 20.0;
                (
                    *item,
                    Rect::from_min_max((0.0, top).into(), (100.0, top + 20.0).into()),
                )
            })
            .collect();
        (categories, rects)
    }

    fn drop(dragged: Item, top: f32) -> (Vec<(u8, Vec<u8>)>, bool, Target) {
        let (mut categories, rects) = sample();
        let target = target(&categories, &rects, dragged, top, 0.0).unwrap();
        let changed = place(&mut categories, dragged, target.slot);
        (ids(&categories), changed, target)
    }

    #[test]
    fn edge_scroll_speed_increases_toward_the_edge() {
        assert_eq!(scroll_speed(50.0), 0.0);
        assert_eq!(scroll_speed(40.0), 0.0);
        assert_eq!(scroll_speed(20.0), 300.0);
        assert_eq!(scroll_speed(0.0), 600.0);
    }

    #[test]
    fn scrolling_updates_the_drop_slot_and_slide_offsets() {
        let (categories, rects) = sample();
        let channel = Item::Channel([11; 32]);
        let before = target(&categories, &rects, channel, 25.0, 0.0).unwrap();
        let after = target(&categories, &rects, channel, 25.0, 20.0).unwrap();
        assert_eq!(before.slot, Slot::Channel(0, 0));
        assert_eq!(after.slot, Slot::Channel(0, 1));
        assert_eq!(after.offsets[&Item::Channel([12; 32])], -20.0);

        let category = Item::Category([2; 32]);
        let after = target(&categories, &rects, category, 40.0, -20.0).unwrap();
        assert_eq!(after.slot, Slot::Category(0));
        assert_eq!(after.offsets[&Item::Channel([12; 32])], 40.0);
    }

    #[test]
    fn dragging_moves_channels_within_and_across_categories() {
        let channel = Item::Channel([11; 32]);
        let (order, changed, target) = drop(channel, 35.0);
        assert!(changed);
        assert_eq!(order, [(1, vec![12, 11]), (2, vec![21])]);
        assert_eq!(target.offsets[&Item::Channel([12; 32])], -20.0);
        assert_eq!(target.offsets[&Item::Category([2; 32])], 0.0);
        assert_eq!(target.hidden, [channel]);

        let (order, _, target) = drop(channel, 75.0);
        assert_eq!(order, [(1, vec![12]), (2, vec![21, 11])]);
        assert_eq!(target.offsets[&Item::Channel([21; 32])], -20.0);

        // Just below the second header: first in that category.
        let (order, _, _) = drop(channel, 55.0);
        assert_eq!(order, [(1, vec![12]), (2, vec![11, 21])]);

        // Above everything: first in the first category.
        let (order, _, target) = drop(Item::Channel([21; 32]), -5.0);
        assert_eq!(order, [(1, vec![21, 11, 12]), (2, vec![])]);
        assert_eq!(target.offsets[&Item::Category([1; 32])], 20.0);
    }

    #[test]
    fn dragging_a_category_moves_its_channels_too() {
        let (order, changed, target) = drop(Item::Category([2; 32]), 10.0);
        assert!(changed);
        assert_eq!(order, [(2, vec![21]), (1, vec![11, 12])]);
        assert_eq!(target.hidden.len(), 2);
        assert_eq!(target.offsets[&Item::Channel([12; 32])], 40.0);
    }

    #[test]
    fn letting_go_where_it_started_changes_nothing() {
        let (order, changed, target) = drop(Item::Channel([11; 32]), 20.0);
        assert!(!changed);
        assert_eq!(order, [(1, vec![11, 12]), (2, vec![21])]);
        assert!(target.offsets.values().all(|&shift| shift == 0.0));
        let (_, changed, _) = drop(Item::Category([1; 32]), 0.0);
        assert!(!changed);
    }

    #[test]
    fn move_up_and_down_step_through_categories() {
        let (mut categories, _) = sample();
        let first = Item::Channel([11; 32]);
        assert_eq!(step(&categories, first, true), None);
        assert_eq!(step(&categories, Item::Category([1; 32]), true), None);
        assert_eq!(step(&categories, Item::Category([2; 32]), false), None);
        let down = step(&categories, Item::Channel([12; 32]), false).unwrap();
        assert!(place(&mut categories, Item::Channel([12; 32]), down));
        assert_eq!(ids(&categories), [(1, vec![11]), (2, vec![12, 21])]);
        let up = step(&categories, Item::Channel([12; 32]), true).unwrap();
        assert!(place(&mut categories, Item::Channel([12; 32]), up));
        assert_eq!(ids(&categories), [(1, vec![11, 12]), (2, vec![21])]);
        let up = step(&categories, Item::Category([2; 32]), true).unwrap();
        assert!(place(&mut categories, Item::Category([2; 32]), up));
        assert_eq!(ids(&categories), [(2, vec![21]), (1, vec![11, 12])]);
    }

    #[test]
    fn unsaveable_without_a_category() {
        let mut loose = category(1, &[11]);
        loose.id = None;
        assert!(order(&[loose]).is_none());
    }

    #[test]
    fn hidden_categories_and_channels_keep_their_places() {
        let id = |b: u8| [b; 32];
        // Category 3 is empty, so nobody sees it; channel 13 is unreadable here.
        let categories = [id(1), id(3), id(2)];
        let channels = [
            (id(11), id(1)),
            (id(13), id(1)),
            (id(12), id(1)),
            (id(21), id(2)),
        ];
        let shown = [(id(2), vec![id(12), id(21)]), (id(1), vec![id(11)])];
        let saved = merge(&shown, &categories, &channels);
        assert_eq!(
            saved,
            [
                (id(2), vec![id(12), id(21)]),
                (id(3), vec![]),
                (id(1), vec![id(11), id(13)]),
            ]
        );
    }
}
