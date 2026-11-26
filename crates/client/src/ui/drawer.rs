//! A side drawer for phones: pulled in from the left edge with a finger, pushed out
//! with a drag to the left or a tap on the dimmed content beside it.
//!
//! Gestures are read from raw pointer input instead of a widget, so taps and vertical
//! scrolls near the edge still reach the content. Only a drag that starts within
//! `EDGE` of the left edge opens it; once it is clearly horizontal the drawer takes it
//! over from whatever widget had it.

use eframe::egui::{
    Align2, Area, AreaState, Context, Id, LayerId, Order, Pos2, Rangef, Rect, Sense, Ui, UiBuilder,
    emath::easing, lerp,
};

use super::SCRIM;

/// How close to the left edge a drag must start to pull the drawer in.
pub const EDGE: f32 = 24.0;
/// Android leaves the app at most this many dp of each screen edge (system gesture
/// exclusion); the rest of the left edge stays its Back gesture. There the drawer
/// starts only in a centred band this tall, the part the app asks Android for.
const ANDROID_BAND_DP: f32 = 200.0;
/// The strip of dimmed content left beside the open drawer, to tap or drag it shut.
pub const PEEK: f32 = 56.0;
/// How far a finger moves before the gesture counts as a drag, not a tap.
const SLOP: f32 = 8.0;
/// A release faster than this, in points per second, opens or closes whatever the
/// position.
const FLING: f32 = 500.0;
/// A full open or close; shorter distances take proportionally less.
const SECONDS: f32 = 0.22;

#[derive(Clone, Copy)]
struct Drag {
    origin: Pos2,
    /// Pulling in from the edge, rather than pushing out.
    opening: bool,
    /// Clearly horizontal: the drawer follows the finger.
    claimed: bool,
    velocity: f32,
}

#[derive(Clone, Copy)]
struct State {
    /// 0 closed, 1 open.
    progress: f32,
    /// What `open` was last frame, to notice a button or a pick changing it.
    open: bool,
    drag: Option<Drag>,
    /// The current settle animation, eased from `from` to `open`.
    from: f32,
    seconds: f32,
}

/// The drawer this frame; `progress` is 0 when closed and 1 when open.
pub struct Drawer {
    id: Id,
    area: Rect,
    pub progress: f32,
}

impl Drawer {
    /// Read this frame's gestures and move the drawer. `open` is where it rests: a
    /// button may change it, and a finished gesture does. `area` is the space it
    /// covers; off (`enabled` false) it jumps to `open` and ignores the pointer.
    pub fn new(ctx: &Context, id: Id, area: Rect, enabled: bool, open: &mut bool) -> Self {
        let target = |open: bool| if open { 1.0 } else { 0.0 };
        let mut state = ctx
            .data(|data| data.get_temp::<State>(id))
            .unwrap_or(State {
                progress: target(*open),
                open: *open,
                drag: None,
                from: target(*open),
                seconds: 0.0,
            });
        let phase = id.with("phase");
        let width = width(area);
        let max_band = if cfg!(target_os = "android") {
            // egui points are dp unless the app zooms.
            ANDROID_BAND_DP / ctx.zoom_factor()
        } else {
            f32::INFINITY
        };
        let band = edge_band(area, max_band);
        let settle = |state: &mut State, open: bool| {
            state.open = open;
            state.from = state.progress;
            let animate = ctx.global_style().animation_time > 0.0;
            state.seconds = if animate {
                SECONDS * (target(open) - state.progress).abs().max(0.3)
            } else {
                0.0
            };
            ctx.animate_value_with_time(phase, 0.0, 0.0);
        };
        if !enabled {
            state.drag = None;
            state.open = *open;
            state.progress = target(*open);
            state.from = state.progress;
            state.seconds = 0.0;
        } else {
            if *open != state.open {
                state.drag = None;
                settle(&mut state, *open);
            }
            let (pressed, down, pos, velocity) = ctx.input(|input| {
                let pointer = &input.pointer;
                (
                    pointer.primary_pressed(),
                    pointer.primary_down(),
                    pointer.interact_pos(),
                    pointer.velocity().x,
                )
            });
            let resting = state.progress == target(*open);
            if pressed
                && resting
                && let Some(origin) = pos.filter(|pos| area.y_range().contains(pos.y))
            {
                // Something floating above the drawer, like the phone self view, keeps its drag.
                let above = ctx
                    .layer_id_at(origin)
                    .is_some_and(|layer| layer.order > Order::Middle);
                let start = !above
                    && if *open {
                        area.contains(origin)
                    } else {
                        origin.x <= area.left() + EDGE && band.contains(origin.y)
                    };
                state.drag = start.then_some(Drag {
                    origin,
                    opening: !*open,
                    claimed: false,
                    velocity: 0.0,
                });
            }
            if let Some(mut drag) = state.drag {
                match pos.filter(|_| down) {
                    Some(pos) => {
                        let delta = pos - drag.origin;
                        if !drag.claimed {
                            let toward = if drag.opening { delta.x } else { -delta.x };
                            if toward > SLOP && delta.x.abs() > delta.y.abs() {
                                drag.claimed = true;
                                // Whatever took the press (a scroll area, a row) lets go.
                                ctx.stop_dragging();
                            } else if delta.length() > SLOP {
                                state.drag = None;
                            }
                        }
                        if drag.claimed && width > 0.0 {
                            drag.velocity = velocity;
                            state.progress = if drag.opening {
                                (pos.x - area.left()) / width
                            } else {
                                1.0 + delta.x / width
                            }
                            .clamp(0.0, 1.0);
                        }
                        if state.drag.is_some() {
                            state.drag = Some(drag);
                        }
                    }
                    None => {
                        state.drag = None;
                        if drag.claimed {
                            *open = if drag.velocity.abs() > FLING {
                                drag.velocity > 0.0
                            } else {
                                state.progress > 0.5
                            };
                            settle(&mut state, *open);
                        }
                    }
                }
            }
            if !state.drag.is_some_and(|drag| drag.claimed) {
                let t = ctx.animate_value_with_time(phase, 1.0, state.seconds);
                state.progress = lerp(state.from..=target(state.open), easing::cubic_out(t));
            }
        }
        // Where an edge drag may start this frame, for Android to keep its Back
        // gesture out of; nothing once it is fully open.
        let edge = (enabled && state.progress < 1.0)
            .then(|| Rect::from_x_y_ranges(area.left()..=area.left() + EDGE, band));
        ctx.data_mut(|data| {
            data.insert_temp(id, state);
            data.insert_temp(edge_id(), edge);
        });
        Self {
            id,
            area,
            progress: state.progress,
        }
    }

    /// Paint the drawer with `add_contents` inside, over the dimmed content. Returns
    /// whether the dimmed content was tapped, which should close it.
    pub fn show(&self, ctx: &Context, add_contents: impl FnOnce(&mut Ui)) -> bool {
        if self.progress <= 0.0 {
            return false;
        }
        let area = self.area;
        let width = width(area);
        let left = area.left() - width * (1.0 - self.progress);
        let drawer = Rect::from_x_y_ranges(left..=left + width, area.y_range());
        let id = self.id.with("area");
        let layer = LayerId::new(Order::Middle, id);
        // Known size up front: a new area would otherwise skip a frame to measure itself.
        ctx.memory_mut(|memory| {
            if memory.area_rect(id).is_none() {
                memory.areas_mut().set_state(
                    layer,
                    AreaState {
                        pivot_pos: Some(area.min),
                        pivot: Align2::LEFT_TOP,
                        size: Some(area.size()),
                        interactable: true,
                        last_became_visible_at: None,
                    },
                );
            }
        });
        Area::new(id)
            .order(Order::Middle)
            .fixed_pos(area.min)
            .constrain(false)
            .fade_in(false)
            .show(ctx, |ui| {
                ui.set_clip_rect(area);
                let scrim = Rect::from_x_y_ranges(drawer.right()..=area.right(), area.y_range());
                // Takes taps and drags off the content underneath.
                let tapped = ui
                    .interact(scrim, self.id.with("scrim"), Sense::click_and_drag())
                    .clicked();
                ui.painter()
                    .rect_filled(scrim, 0.0, SCRIM.gamma_multiply(self.progress));
                ui.scope_builder(UiBuilder::new().max_rect(drawer), add_contents);
                tapped
            })
            .inner
    }
}

fn edge_id() -> Id {
    Id::unique("drawer edge")
}

/// The strip a drawer took edge drags in during this frame, if any, forgotten on
/// read: a frame without a drawer (another screen, a wide window) gives `None`.
#[cfg(any(target_os = "android", test))]
pub fn take_edge(ctx: &Context) -> Option<Rect> {
    ctx.data_mut(|data| data.remove_temp::<Option<Rect>>(edge_id()))
        .flatten()
}

/// The part of `area`'s height where an edge drag may start: all of it, or a
/// centred band at most `max` tall.
fn edge_band(area: Rect, max: f32) -> Rangef {
    let half = area.height().min(max) / 2.0;
    let middle = area.center().y;
    Rangef::new(middle - half, middle + half)
}

/// How wide the open drawer is in `area`.
pub fn width(area: Rect) -> f32 {
    (area.width() - PEEK).max(0.0)
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Event, Modifiers, PointerButton, ScrollArea, Vec2, pos2, vec2};
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    const SIZE: Vec2 = Vec2::new(390.0, 844.0);

    struct Screen {
        open: bool,
        taps: usize,
    }

    fn harness(open: bool) -> Harness<'static, Screen> {
        Harness::builder()
            .with_size(SIZE)
            .with_step_dt(1.0 / 60.0)
            .with_max_steps(120)
            .build_ui_state(
                |ui, screen: &mut Screen| {
                    let area = ui.max_rect();
                    let drawer =
                        Drawer::new(ui.ctx(), Id::unique("drawer"), area, true, &mut screen.open);
                    // A wide row that scrolls sideways, like a code block.
                    ScrollArea::horizontal().show(ui, |ui| {
                        ui.add_sized(vec2(2000.0, 200.0), eframe::egui::Label::new("wide"));
                    });
                    if ui.button("Content").clicked() {
                        screen.taps += 1;
                    }
                    if drawer.show(ui.ctx(), |ui| {
                        ui.label("Channels");
                    }) {
                        screen.open = false;
                    }
                },
                Screen { open, taps: 0 },
            )
    }

    fn press(h: &mut Harness<'_, Screen>, pos: Pos2, pressed: bool) {
        h.input_mut().events.push(Event::PointerMoved(pos));
        h.input_mut().events.push(Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        });
        h.step();
    }

    /// Drag from `from` to `to` in small, slow steps, then hold still before letting
    /// go, so the release carries no fling.
    fn drag(h: &mut Harness<'_, Screen>, from: Pos2, to: Pos2) {
        press(h, from, true);
        for i in 1..=20 {
            let at = from + (to - from) * (i as f32 / 20.0);
            h.input_mut().events.push(Event::PointerMoved(at));
            h.step();
        }
        for _ in 0..10 {
            h.step();
        }
        press(h, to, false);
        h.run();
    }

    fn progress(h: &Harness<'_, Screen>) -> f32 {
        h.ctx
            .data(|data| data.get_temp::<State>(Id::unique("drawer")))
            .unwrap()
            .progress
    }

    #[test]
    fn a_short_drag_from_the_edge_snaps_back() {
        let mut h = harness(false);
        h.run();
        let from = pos2(10.0, 400.0);
        press(&mut h, from, true);
        for x in [30.0, 60.0, 100.0] {
            h.input_mut()
                .events
                .push(Event::PointerMoved(pos2(x, 400.0)));
            h.step();
        }
        // Following the finger.
        let followed = progress(&h);
        assert!(followed > 0.2 && followed < 0.35, "{followed}");
        for _ in 0..10 {
            h.step();
        }
        press(&mut h, pos2(100.0, 400.0), false);
        h.run();
        assert!(!h.state().open);
        assert_eq!(progress(&h), 0.0);
    }

    #[test]
    fn a_drag_past_half_opens_and_a_drag_left_closes() {
        let mut h = harness(false);
        h.run();
        // Over the sideways scroll area: the edge wins.
        drag(&mut h, pos2(8.0, 100.0), pos2(250.0, 100.0));
        assert!(h.state().open);
        assert_eq!(progress(&h), 1.0);
        // Left across the drawer itself.
        drag(&mut h, pos2(300.0, 300.0), pos2(80.0, 300.0));
        assert!(!h.state().open);
        assert_eq!(progress(&h), 0.0);
    }

    #[test]
    fn a_quick_flick_opens_from_a_short_drag() {
        let mut h = harness(false);
        h.run();
        press(&mut h, pos2(5.0, 400.0), true);
        for x in [40.0, 80.0, 120.0] {
            h.input_mut()
                .events
                .push(Event::PointerMoved(pos2(x, 400.0)));
            h.step();
        }
        press(&mut h, pos2(120.0, 400.0), false);
        h.run();
        assert!(h.state().open);
    }

    #[test]
    fn a_tap_on_the_dimmed_content_closes_it() {
        let mut h = harness(true);
        h.run();
        let beside = pos2(SIZE.x - PEEK / 2.0, 400.0);
        press(&mut h, beside, true);
        press(&mut h, beside, false);
        h.run();
        assert!(!h.state().open);
        assert_eq!(progress(&h), 0.0);
    }

    #[test]
    fn drags_away_from_the_edge_are_left_alone() {
        let mut h = harness(false);
        h.run();
        drag(&mut h, pos2(120.0, 100.0), pos2(380.0, 100.0));
        assert!(!h.state().open);
        assert_eq!(progress(&h), 0.0);
        // A vertical drag from the edge does not open it either.
        drag(&mut h, pos2(8.0, 300.0), pos2(20.0, 700.0));
        assert!(!h.state().open);
        // Taps near the edge still reach the content.
        let rect = h.get_by_label("Content").rect();
        let at = pos2(rect.left() + 2.0, rect.center().y);
        assert!(at.x <= EDGE);
        press(&mut h, at, true);
        press(&mut h, at, false);
        h.run();
        assert_eq!(h.state().taps, 1);
    }

    /// A floating picture above the open drawer, like the phone self view (#656):
    /// dragging it left moves it and leaves the drawer open.
    #[test]
    fn a_drag_on_something_floating_above_is_left_to_it() {
        let float = Rect::from_min_size(pos2(250.0, 600.0), vec2(100.0, 120.0));
        let mut h = Harness::builder()
            .with_size(SIZE)
            .with_step_dt(1.0 / 60.0)
            .with_max_steps(120)
            .build_ui_state(
                move |ui, screen: &mut Screen| {
                    let area = ui.max_rect();
                    let drawer =
                        Drawer::new(ui.ctx(), Id::unique("drawer"), area, true, &mut screen.open);
                    if drawer.show(ui.ctx(), |ui| {
                        ui.label("Channels");
                    }) {
                        screen.open = false;
                    }
                    Area::new(Id::unique("float"))
                        .order(Order::Foreground)
                        .fixed_pos(float.min)
                        .show(ui.ctx(), |ui| {
                            let response = ui.allocate_rect(float, Sense::drag());
                            if response.dragged() {
                                screen.taps += 1;
                            }
                        });
                },
                Screen {
                    open: true,
                    taps: 0,
                },
            );
        h.run();
        drag(&mut h, float.center(), pos2(20.0, float.center().y));
        assert!(h.state().open, "the drawer stays open");
        assert_eq!(progress(&h), 1.0);
        assert!(h.state().taps > 0, "the floating picture got the drag");
    }

    #[test]
    fn the_edge_band_is_centred_and_capped() {
        let area = Rect::from_min_size(pos2(0.0, 40.0), SIZE);
        assert_eq!(edge_band(area, f32::INFINITY), area.y_range());
        let band = edge_band(area, 200.0);
        assert_eq!(band.span(), 200.0);
        assert_eq!(band.center(), area.center().y);
    }

    #[test]
    fn the_edge_is_reported_until_fully_open() {
        let mut h = harness(false);
        h.run();
        let edge = take_edge(&h.ctx).expect("closed: the edge takes drags");
        assert_eq!(edge.width(), EDGE);
        assert_eq!(take_edge(&h.ctx), None, "forgotten once read");
        h.state_mut().open = true;
        h.run();
        assert_eq!(take_edge(&h.ctx), None);
    }

    #[test]
    fn a_button_opens_and_closes_it_with_an_animation() {
        let mut h = harness(false);
        // The harness turns animations off; with them off the drawer jumps.
        h.ctx
            .global_style_mut(|style| style.animation_time = 1.0 / 12.0);
        h.run();
        h.state_mut().open = true;
        h.step();
        h.step();
        let moving = progress(&h);
        assert!(moving > 0.0 && moving < 1.0, "{moving}");
        h.run();
        assert_eq!(progress(&h), 1.0);
        h.state_mut().open = false;
        h.step();
        h.step();
        let moving = progress(&h);
        assert!(moving > 0.0 && moving < 1.0, "{moving}");
        h.run();
        assert_eq!(progress(&h), 0.0);
    }
}
