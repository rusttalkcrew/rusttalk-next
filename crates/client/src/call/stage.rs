//! The call stage in a voice channel: a tile per person and per shared screen, focus,
//! minimized bubbles, t!("call.show_only_video"), presentation mode and full screen.

use std::ops::RangeInclusive;

use eframe::egui::{
    self, Align, Align2, CursorIcon, FontId, Id as UiId, Image, Key, Layout, Rect, Role, Sense,
    Stroke, StrokeKind, TextureId, Ui, UiBuilder, Vec2, WidgetInfo, pos2,
    text::{LayoutJob, TextWrapping},
    vec2,
};
use rusttalk_core::client::voice::VoiceStatus;

use super::{
    Sharing, Voice, call_server, join, mixer,
    overlay::{self, Overlay, Row},
    remote::Control,
    ring, sound_badges, stop_watching, upload, watch,
};
use crate::{
    friends::Relation,
    home::{
        Home, face,
        person::{self, Person, Pick},
    },
    net::Command,
    server::{Id, Peer, Screen, Server},
    ui::{self, Look},
};
use rusttalk_core::control::Input;

/// Tiles keep this shape in the grid and the strip.
const ASPECT: f32 = 16.0 / 9.0;
const GAP: f32 = 8.0;
/// A minimized person's round picture.
pub const BUBBLE: f32 = 40.0;
/// The strip's share of the stage beside a focused tile or a presented screen.
const FOCUS_STRIP: f32 = 0.24;
const PRESENT_STRIP: f32 = 0.16;
/// Stages wider than this many times their height get the strip on the right.
const SIDE_STRIP: f32 = 2.5;
/// Lower tiles show the picture at the left with the name beside it.
const COMPACT: f32 = 100.0;
/// How thick the strip can get.
const STRIP_RANGE: RangeInclusive<f32> = 56.0..=160.0;
const TOOL: Vec2 = Vec2::splat(28.0);
/// The badge after a tile's name.
const BADGE: f32 = 22.0;

/// What a tile shows: a person (their camera or picture), a shared screen, or the
/// preview of our own.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Tile {
    Person(Id),
    Screen(String),
    Preview,
}

/// The stage's choices for this call only; they reset when it ends.
#[derive(Default)]
pub struct Stage {
    focus: Option<Tile>,
    /// People shown as bubbles, in the order they were minimized.
    minimized: Vec<Id>,
    presenting: bool,
    /// Covers the whole window.
    pub full: Option<Tile>,
}

impl Stage {
    /// Stopping a screen also drops its big tile: a big screen is received on its
    /// own (see `stage`), so keeping it big would start watching again at once.
    fn stop_watching(&mut self) {
        self.presenting = false;
        if matches!(self.focus, Some(Tile::Screen(_))) {
            self.focus = None;
        }
    }
}

/// Tiles of `count` in a grid as large as fits `area`, rows centred.
pub fn grid(count: usize, area: Rect, gap: f32) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let width = |cols: usize| {
        let rows = count.div_ceil(cols);
        let wide = (area.width() - gap * (cols - 1) as f32) / cols as f32;
        let high = (area.height() - gap * (rows - 1) as f32) / rows as f32;
        wide.min(high * ASPECT)
    };
    let cols = (1..=count)
        .max_by(|a, b| width(*a).total_cmp(&width(*b)).then(b.cmp(a)))
        .unwrap_or(1);
    let w = width(cols).max(1.0);
    let h = w / ASPECT;
    let rows = count.div_ceil(cols);
    let top = area.center().y - (rows as f32 * h + (rows - 1) as f32 * gap) / 2.0;
    (0..count)
        .map(|at| {
            let (row, col) = (at / cols, at % cols);
            let in_row = (count - row * cols).min(cols) as f32;
            let left = area.center().x - (in_row * w + (in_row - 1.0) * gap) / 2.0;
            let min = pos2(left + col as f32 * (w + gap), top + row as f32 * (h + gap));
            Rect::from_min_size(min, vec2(w, h))
        })
        .collect()
}

/// One big tile and a strip of `others`: along the bottom, or down the right of a
/// stage much wider than high. `share` is the strip's part of the stage.
pub fn spotlight(others: usize, area: Rect, gap: f32, share: f32) -> (Rect, Vec<Rect>) {
    if others == 0 {
        return (area, Vec::new());
    }
    // Tiles are wide, so a strip at the bottom suits all but very wide stages.
    let bottom = area.width() <= area.height() * SIDE_STRIP;
    let side = if bottom { area.height() } else { area.width() };
    let thick = (side * share).clamp(*STRIP_RANGE.start(), *STRIP_RANGE.end());
    let (big, strip) = if bottom {
        let y = area.bottom() - thick;
        (
            Rect::from_min_max(area.min, pos2(area.right(), y - gap)),
            Rect::from_min_max(pos2(area.left(), y), area.max),
        )
    } else {
        let x = area.right() - thick;
        (
            Rect::from_min_max(area.min, pos2(x - gap, area.bottom())),
            Rect::from_min_max(pos2(x, area.top()), area.max),
        )
    };
    (big, line(others, strip, gap, bottom))
}

/// `count` tiles in a row (or column), shrunk to fit and centred.
fn line(count: usize, area: Rect, gap: f32, row: bool) -> Vec<Rect> {
    let n = count as f32;
    let (w, h) = if row {
        let w = (area.height() * ASPECT).min((area.width() - gap * (n - 1.0)) / n);
        (w, w / ASPECT)
    } else {
        let h = (area.width() / ASPECT).min((area.height() - gap * (n - 1.0)) / n);
        (h * ASPECT, h)
    };
    let (w, h) = (w.max(1.0), h.max(1.0));
    (0..count)
        .map(|at| {
            let at = at as f32;
            let min = if row {
                let left = area.center().x - (n * w + (n - 1.0) * gap) / 2.0;
                pos2(left + at * (w + gap), area.center().y - h / 2.0)
            } else {
                let top = area.center().y - (n * h + (n - 1.0) * gap) / 2.0;
                pos2(area.center().x - w / 2.0, top + at * (h + gap))
            };
            Rect::from_min_size(min, vec2(w, h))
        })
        .collect()
}

fn bubbles_per_row(stage: Rect, gap: f32) -> usize {
    (((stage.width() - gap) / (BUBBLE + gap)).floor() as usize).max(1)
}

/// Bubble `at` from the stage's bottom-left corner: left to right, then upwards.
pub fn bubble(at: usize, stage: Rect, gap: f32) -> Rect {
    let per_row = bubbles_per_row(stage, gap);
    let (row, col) = ((at / per_row) as f32, (at % per_row) as f32);
    let min = pos2(
        stage.left() + gap + col * (BUBBLE + gap),
        stage.bottom() - gap - BUBBLE - row * (BUBBLE + gap),
    );
    Rect::from_min_size(min, Vec2::splat(BUBBLE))
}

/// The height the bubble rows take off the bottom of the stage.
pub fn bubbles_height(count: usize, stage: Rect, gap: f32) -> f32 {
    let rows = count.div_ceil(bubbles_per_row(stage, gap));
    rows as f32 * (BUBBLE + gap)
}

/// A texture and how to fit it in a tile.
#[derive(Clone, Copy)]
struct Picture {
    texture: TextureId,
    size: Vec2,
    /// The self view, shown like a mirror.
    mirrored: bool,
    /// Fill the tile and crop (cameras) instead of showing it whole (screens).
    cover: bool,
}

/// The texture part that fills `area` without stretching.
fn cover_uv(size: Vec2, area: Vec2) -> Rect {
    let (picture, tile) = (size.x / size.y, area.x / area.y);
    if picture > tile {
        let part = tile / picture;
        Rect::from_x_y_ranges((1.0 - part) / 2.0..=(1.0 + part) / 2.0, 0.0..=1.0)
    } else {
        let part = picture / tile;
        Rect::from_x_y_ranges(0.0..=1.0, (1.0 - part) / 2.0..=(1.0 + part) / 2.0)
    }
}

/// Where a picture of `size` shows whole in `rect`.
fn fitted(rect: Rect, size: Vec2) -> Rect {
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    Rect::from_center_size(rect.center(), size * scale)
}

fn paint_picture(ui: &Ui, rect: Rect, picture: Picture) {
    let (rect, mut uv) = if picture.cover {
        (rect, cover_uv(picture.size, rect.size()))
    } else {
        let whole = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        (fitted(rect, picture.size), whole)
    };
    if picture.mirrored {
        uv = Rect::from_min_max(pos2(uv.max.x, uv.min.y), pos2(uv.min.x, uv.max.y));
    }
    Image::new((picture.texture, picture.size))
        .uv(uv)
        .corner_radius(ui::RADIUS)
        .paint_at(ui, rect);
}

/// A receiving camera's newest picture, once one arrived.
fn camera_picture(ctx: &egui::Context, voice: &mut Voice, sid: &str) -> Option<Picture> {
    let slot = voice.cameras.get_mut(sid)?;
    if let Some(image) = voice.video.take(sid) {
        upload(ctx, slot, sid, image);
    }
    let texture = slot.as_ref()?;
    Some(Picture {
        texture: texture.id(),
        size: texture.size_vec2(),
        mirrored: false,
        cover: true,
    })
}

/// The watched screen's newest picture, if `sid` is watched and one arrived.
fn screen_picture(ctx: &egui::Context, voice: &mut Voice, sid: &str) -> Option<Picture> {
    let watching = voice.watching.as_mut().filter(|w| w.sid == sid)?;
    if let Some(image) = voice.video.take(sid) {
        upload(ctx, &mut watching.texture, "screen", image);
    }
    let texture = watching.texture.as_ref()?;
    Some(Picture {
        texture: texture.id(),
        size: texture.size_vec2(),
        mirrored: false,
        cover: false,
    })
}

/// Our own share's newest picture, from the capture. Its copies don't ask for a
/// repaint, so the window looks again at their rate.
fn preview_picture(ctx: &egui::Context, voice: &mut Voice) -> Option<Picture> {
    ctx.request_repaint_after(std::time::Duration::from_millis(1000 / 15));
    if let Some(image) = voice.video.take_preview() {
        upload(ctx, &mut voice.preview, "preview", image);
    }
    let texture = voice.preview.as_ref()?;
    Some(Picture {
        texture: texture.id(),
        size: texture.size_vec2(),
        mirrored: false,
        cover: false,
    })
}

/// Whether our screen's preview is on the stage.
fn previewing(voice: &Voice) -> bool {
    matches!(voice.sharing, Sharing::On | Sharing::Changing) && voice.video.preview.is_on()
}

/// A person's camera picture: the self view for us, else their track, which this
/// marks as drawn so it stays received.
fn person_picture(
    ctx: &egui::Context,
    voice: &mut Voice,
    peer: &Peer,
    me: bool,
) -> Option<Picture> {
    let sid = peer.camera.as_ref()?;
    if me {
        let texture = voice.camera.self_texture(ctx)?;
        return Some(Picture {
            texture: texture.id(),
            size: texture.size_vec2(),
            mirrored: true,
            cover: true,
        });
    }
    if voice.hidden.contains(&peer.person) {
        return None;
    }
    voice.shown.insert(sid.clone());
    camera_picture(ctx, voice, sid)
}

/// Something on the stage, gathered before drawing.
struct Entry<'p> {
    tile: Tile,
    peer: &'p Peer,
    name: String,
    me: bool,
    picture: Option<Picture>,
    /// A camera that is on, or a screen; what t!("call.show_only_video") keeps.
    video: bool,
    /// A screen that is received now.
    watched: bool,
    speaking: bool,
}

/// What a tile asked for this frame.
enum Act {
    Focus(Tile),
    Unfocus,
    Minimize(Id),
    Full(Tile),
    Watch(Screen),
    StopWatching,
    HidePreview,
    ToggleFps,
    ToggleNerdStats,
    /// Picked on someone's card or in their menu.
    Person(Id, Pick),
    RequestControl(String),
    ReleaseControl,
    /// Input for the screen we control.
    Control(Vec<Input>),
}

/// The tiles in this room: shared screens first, then people.
fn entries<'p>(
    ctx: &egui::Context,
    voice: &mut Voice,
    server: &Server,
    peers: &'p [Peer],
    joined: bool,
) -> Vec<Entry<'p>> {
    let mut screens = Vec::new();
    let mut people = Vec::new();
    for peer in peers {
        let name = peer.name(server);
        let me = peer.person == server.dir.me;
        let speaking = voice.speaking(server, &peer.person);
        let camera = joined && peer.camera.is_some();
        let video = camera && (me || !voice.hidden.contains(&peer.person));
        let picture = camera
            .then(|| person_picture(ctx, voice, peer, me))
            .flatten();
        // Our own screen isn't received; its preview comes straight from the capture.
        if joined && me && previewing(voice) {
            screens.push(Entry {
                tile: Tile::Preview,
                peer,
                name: t!("call.your_screen_preview").to_owned(),
                me,
                picture: preview_picture(ctx, voice),
                video: true,
                watched: false,
                speaking: false,
            });
        }
        if joined && !me {
            for screen in &peer.screens {
                let watched = voice.watching.as_ref().is_some_and(|w| w.sid == screen.sid);
                screens.push(Entry {
                    tile: Tile::Screen(screen.sid.clone()),
                    peer,
                    name: t!("call.someones_screen", name = name),
                    me,
                    picture: screen_picture(ctx, voice, &screen.sid),
                    video: true,
                    watched,
                    speaking: false,
                });
            }
        }
        people.push(Entry {
            tile: Tile::Person(peer.person),
            peer,
            name,
            me,
            picture,
            video,
            watched: false,
            speaking,
        });
    }
    screens.extend(people);
    screens
}

/// The stage for a voice channel, filling `ui`: tiles, bubbles, its toolbar and,
/// before joining, t!("call.join_voice").
pub fn stage(ui: &mut Ui, home: &mut Home, index: usize, channel: Id) {
    let (rect, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
    let c = ui::colors(ui);
    ui.painter().rect_filled(rect, ui::RADIUS, c.rail);
    let server = &home.servers[index];
    let joined = home.voice.in_call(server, &channel);
    let peers = server.voice.get(&channel).cloned().unwrap_or_default();
    let ctx = ui.ctx().clone();
    let voice = &mut home.voice;
    let video_only = &mut home.layout.split.video_only;
    let all = entries(&ctx, voice, server, &peers, joined);
    let stage = &mut voice.stage;
    stage
        .minimized
        .retain(|person| peers.iter().any(|p| p.person == *person));
    let presentable = all
        .iter()
        .position(|e| matches!(e.tile, Tile::Screen(_) | Tile::Preview));
    if presentable.is_none() {
        stage.presenting = false;
    }

    let mut acts = Vec::new();
    // The toolbar, top right.
    let bar = Rect::from_min_size(
        pos2(rect.right() - GAP - 2.0 * TOOL.x - 4.0, rect.top() + GAP),
        vec2(2.0 * TOOL.x + 4.0, TOOL.y),
    );
    ui.scope_builder(
        UiBuilder::new()
            .max_rect(bar)
            .layout(Layout::right_to_left(Align::Center)),
        |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let on = Look::On(ui::accent(ui).base);
            let (tip, look) = if stage.presenting {
                (t!("call.leave_presentation"), on)
            } else {
                (t!("call.presentation_mode"), Look::Plain)
            };
            let present = ui.add_enabled_ui(joined && presentable.is_some(), |ui| {
                ui::icon_button(ui, "🖥", tip, look, TOOL)
            });
            let present = present
                .inner
                .on_disabled_hover_text(t!("call.presentation_help"));
            if present.clicked() {
                stage.presenting ^= true;
                stage.focus = None;
            }
            let (tip, look) = if *video_only {
                (t!("call.show_everyone"), on)
            } else {
                (t!("call.show_only_video"), Look::Plain)
            };
            if ui::icon_button(ui, "📷", tip, look, TOOL).clicked() {
                *video_only ^= true;
            }
        },
    );

    let shown: Vec<&Entry> = all
        .iter()
        .filter(|e| match e.tile {
            Tile::Person(person) => !stage.minimized.contains(&person),
            Tile::Screen(_) | Tile::Preview => true,
        })
        .filter(|e| !*video_only || e.video)
        .collect();
    let bubbles: Vec<&Entry> = stage
        .minimized
        .iter()
        .filter_map(|person| all.iter().find(|e| e.tile == Tile::Person(*person)))
        .collect();

    // Tiles go between the toolbar and the bubbles (or the join button).
    let mut area = rect.shrink(GAP);
    area.min.y = bar.bottom() + GAP;
    if !bubbles.is_empty() {
        area.max.y -= bubbles_height(bubbles.len(), rect, GAP);
    }
    if !joined {
        area.max.y -= ui::LARGE_CONTROL.y + GAP;
    }

    // The big tile: the presented screen, else the focused tile.
    let big = if stage.presenting {
        let watched = shown.iter().position(|e| e.watched);
        watched.or_else(|| {
            shown
                .iter()
                .position(|e| matches!(e.tile, Tile::Screen(_) | Tile::Preview))
        })
    } else {
        (stage.focus.as_ref()).and_then(|focus| shown.iter().position(|e| e.tile == *focus))
    };
    // A big screen is worth receiving.
    if let Some(entry) = big.map(|at| shown[at])
        && let Tile::Screen(sid) = &entry.tile
        && !entry.watched
        && joined
        && let Some(screen) = entry.peer.screens.iter().find(|s| s.sid == *sid)
    {
        acts.push(Act::Watch(screen.clone()));
    }
    if !stage.presenting && big.is_none() {
        stage.focus = None;
    }
    let share = if stage.presenting {
        PRESENT_STRIP
    } else {
        FOCUS_STRIP
    };
    let targets: Vec<Rect> = match big {
        Some(big) if area.width() > 0.0 && area.height() > 0.0 => {
            let (main, mut strip) = spotlight(shown.len() - 1, area, GAP, share);
            strip.insert(big, main);
            strip
        }
        _ => grid(shown.len(), area, GAP),
    };

    if shown.is_empty() {
        let text = if *video_only && !all.is_empty() {
            t!("call.no_video")
        } else if joined {
            t!("common.connecting")
        } else {
            t!("call.nobody_here")
        };
        ui.painter().text(
            area.center(),
            Align2::CENTER_CENTER,
            text,
            FontId::proportional(ui::TEXT_BODY),
            c.muted,
        );
    }
    let audio = home.voice.audio.clone();
    let split = home.layout.split;
    let stats = home.voice.call.as_ref().map(|call| &call.stats);
    let (stage, control) = (&home.voice.stage, &mut home.voice.control);
    for (at, (entry, target)) in shown.iter().zip(&targets).enumerate() {
        let rect = animate(&ctx, &entry.tile, *target, rect);
        let is_big = big == Some(at);
        let focused = stage.focus.as_ref() == Some(&entry.tile);
        let remote = match &entry.tile {
            Tile::Screen(sid) if joined && !entry.me => control.screen(sid),
            _ => None,
        };
        let state = TileState {
            relation: home.friends.relation(&entry.peer.person),
            joined,
            big: is_big,
            spotlight: big.is_some(),
            focused,
            remote,
        };
        let overlay = Overlay::new(&entry.tile, split.show_fps, split.nerd_stats, stats);
        tile(
            ui, rect, entry, state, server, &audio, control, &overlay, &mut acts,
        );
    }
    for (at, entry) in bubbles.iter().enumerate() {
        let spot = bubble(at, rect, GAP);
        let mut child = ui.new_child(UiBuilder::new().max_rect(spot));
        let picture = face(
            &mut child,
            &server.dir,
            &entry.peer.person,
            &entry.name,
            BUBBLE,
            false,
        );
        if entry.speaking {
            ring(ui, picture.rect);
        }
        let label = t!("call.show_name", name = entry.name);
        let response = ui
            .interact(
                spot,
                UiId::unique(("bubble", entry.peer.person)),
                Sense::click(),
            )
            .on_hover_text(&entry.name)
            .on_hover_cursor(CursorIcon::PointingHand);
        response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
        if response.clicked() {
            acts.push(Act::Minimize(entry.peer.person));
        }
    }
    if !joined {
        let spot = Rect::from_center_size(
            pos2(
                rect.center().x,
                rect.bottom() - GAP - ui::LARGE_CONTROL.y / 2.0,
            ),
            ui::LARGE_CONTROL,
        );
        let join_now = ui
            .scope_builder(
                UiBuilder::new()
                    .max_rect(spot)
                    .layout(Layout::top_down(Align::Center)),
                |ui| ui::primary_large(ui, t!("call.join_voice")).clicked(),
            )
            .inner;
        if join_now {
            join(home, index, channel);
        }
    }
    let now = ui.input(|input| input.time);
    apply(home, index, now, acts);
}

/// A tile's place, eased towards `target` as a share of the stage, so the stage
/// can resize without the tiles lagging behind.
fn animate(ctx: &egui::Context, tile: &Tile, target: Rect, stage: Rect) -> Rect {
    let id = UiId::unique(("stage tile", tile));
    let size = stage.size().max(Vec2::splat(1.0));
    let ease = |key: &str, value: f32, origin: f32, length: f32| {
        let share = (value - origin) / length;
        origin + length * ctx.animate_value_with_time(id.with(key), share, ui::STATE_SECONDS)
    };
    Rect::from_min_max(
        pos2(
            ease("left", target.left(), stage.left(), size.x),
            ease("top", target.top(), stage.top(), size.y),
        ),
        pos2(
            ease("right", target.right(), stage.left(), size.x),
            ease("bottom", target.bottom(), stage.top(), size.y),
        ),
    )
}

#[derive(Clone, Copy)]
struct TileState {
    relation: Relation,
    joined: bool,
    /// The focused or presented tile.
    big: bool,
    /// Someone is big, so the others are in the strip.
    spotlight: bool,
    focused: bool,
    /// This screen is ours to control (`true`), or we asked to.
    remote: Option<bool>,
}

/// One tile: picture or video, speaking outline, name and badges, and its actions
/// while hovered.
#[allow(clippy::too_many_arguments)]
fn tile(
    ui: &mut Ui,
    rect: Rect,
    entry: &Entry,
    state: TileState,
    server: &Server,
    audio: &crate::audio::Audio,
    control: &mut Control,
    overlay: &Overlay,
    acts: &mut Vec<Act>,
) {
    let c = ui::colors(ui);
    ui.painter().rect_filled(rect, ui::RADIUS, c.side);
    let person = entry.peer.person;
    match (&entry.picture, &entry.tile) {
        (Some(picture), _) => paint_picture(ui, rect, *picture),
        (None, Tile::Person(_)) => {
            let spot = avatar_spot(rect);
            let size = spot.width();
            let mut child = ui.new_child(UiBuilder::new().max_rect(spot));
            let picture = face(&mut child, &server.dir, &person, &entry.name, size, false);
            if entry.speaking {
                ring(ui, picture.rect);
            }
        }
        (None, Tile::Screen(_) | Tile::Preview) => {
            let text = if entry.watched || entry.tile == Tile::Preview {
                t!("call.waiting_picture")
            } else if rect.height() > 90.0 {
                t!("call.click_to_watch")
            } else {
                ""
            };
            let painter = ui.painter();
            painter.text(
                rect.center() - vec2(0.0, 10.0),
                Align2::CENTER_CENTER,
                "🖥",
                FontId::proportional(ui::TEXT_ICON_LARGE),
                c.muted,
            );
            painter.text(
                rect.center() + vec2(0.0, 16.0),
                Align2::CENTER_CENTER,
                text,
                FontId::proportional(ui::TEXT_SMALL),
                c.muted,
            );
        }
    }
    if entry.speaking {
        let outline = Stroke::new(2.0, c.success);
        ui.painter()
            .rect_stroke(rect, ui::RADIUS, outline, StrokeKind::Inside);
    }
    // Above the picture, so a click on it lands here.
    let response = ui.interact(rect, UiId::unique(("tile", &entry.tile)), Sense::click());
    let screen = match &entry.tile {
        Tile::Screen(sid) => entry.peer.screens.iter().find(|s| s.sid == *sid),
        Tile::Person(_) | Tile::Preview => None,
    };
    let watch_click = screen.filter(|_| !entry.watched && state.joined);
    let controlling = state.remote == Some(true);
    if controlling {
        // Clicks and keys here go to the shared screen, not to the stage.
        let label = t!("call.control_name", name = entry.name);
        response.widget_info(|| WidgetInfo::labeled(Role::Group, true, &label));
        if let Some(picture) = &entry.picture {
            let (inputs, stop) = control.capture(ui, &response, fitted(rect, picture.size));
            if !inputs.is_empty() {
                acts.push(Act::Control(inputs));
            }
            if stop {
                acts.push(Act::ReleaseControl);
            }
        }
    } else if state.spotlight && !state.big {
        let label = t!("call.focus_name", name = entry.name);
        response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
        if response.clicked() {
            acts.push(Act::Focus(entry.tile.clone()));
        }
    } else if let Some(screen) = watch_click {
        let label = t!("call.watch_name", name = entry.name);
        response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
        if response.clicked() {
            acts.push(Act::Watch(screen.clone()));
        }
    } else {
        let role = match entry.tile {
            Tile::Person(_) => Role::Button,
            Tile::Screen(_) | Tile::Preview => Role::Group,
        };
        response.widget_info(|| WidgetInfo::labeled(role, true, &entry.name));
    }
    if response.double_clicked() && !controlling {
        acts.push(if state.focused {
            Act::Unfocus
        } else {
            Act::Focus(entry.tile.clone())
        });
    }
    if matches!(entry.tile, Tile::Person(_)) && !entry.peer.unknown && !controlling {
        let others = !entry.me && state.joined;
        let target = Person {
            server,
            person,
            relation: state.relation,
            audio: others.then_some(audio),
            video: (others && entry.peer.camera.is_some()).then_some(entry.video),
        };
        // In the strip a click focuses the tile, so only the menu is there.
        let picked = if state.spotlight && !state.big {
            ui::context_menu(&response, |m| target.menu(m)).flatten()
        } else {
            person::interact(ui, &response, &target, true)
        };
        if let Some(picked) = picked {
            acts.push(Act::Person(person, picked));
        }
    }
    if !matches!(entry.tile, Tile::Person(_)) && !controlling {
        // The sharer's own menu sits inside, for someone else's screen.
        let sharer = (!entry.me && state.joined && !entry.peer.unknown).then_some(Person {
            server,
            person,
            relation: state.relation,
            audio: Some(audio),
            video: None,
        });
        let menu = ui::context_menu(&response, |m| {
            screen_menu(m, entry, state, audio, sharer.as_ref(), overlay, acts)
        });
        if let Some(Some(picked)) = menu {
            acts.push(Act::Person(person, picked));
        }
    }
    if let Some(on) = state.remote {
        let sharer = entry.peer.name(server);
        remote_tag(ui, rect, entry, on, response.has_focus(), &sharer);
    }
    overlay.paint(ui, rect);
    name_tag(ui, rect, entry, audio);
    tools(ui, rect, entry, state, audio, acts);
}

/// The right-click menu of a screen tile; the same actions as its hover buttons, its
/// stream sound and the overlays. Returns what was picked in the sharer's submenu.
fn screen_menu(
    m: &mut ui::Menu,
    entry: &Entry,
    state: TileState,
    audio: &crate::audio::Audio,
    sharer: Option<&Person>,
    overlay: &Overlay,
    acts: &mut Vec<Act>,
) -> Option<Pick> {
    let mut picked = None;
    if let Some(sharer) = sharer {
        picked = m
            .submenu("👤", &entry.peer.name(sharer.server), |m| sharer.menu(m))
            .flatten();
        m.separator();
    }
    let sound = state.joined && !entry.me && !entry.peer.apps.is_empty();
    let rows = overlay::rows(
        &entry.tile,
        state.focused,
        entry.picture.is_some(),
        entry.watched,
        sound,
    );
    let person = entry.peer.person;
    for row in rows {
        match row {
            Row::Focus if m.item("🗖", t!("call.focus")).clicked() => {
                acts.push(Act::Focus(entry.tile.clone()));
            }
            Row::Unfocus if m.item("🗗", t!("call.unfocus")).clicked() => acts.push(Act::Unfocus),
            Row::FullScreen if m.item("⛶", t!("call.full_screen")).clicked() => {
                acts.push(Act::Full(entry.tile.clone()));
            }
            Row::StopWatching if m.danger("⏹", t!("call.stop_watching")).clicked() => {
                acts.push(Act::StopWatching);
            }
            Row::HidePreview if m.danger("⏹", t!("call.hide_preview")).clicked() => {
                acts.push(Act::HidePreview);
            }
            Row::Volume => {
                m.separator();
                // The sharer's apps, whose sound arrives with the screen.
                mixer::show(m, audio, person, false, &entry.peer.apps);
            }
            Row::ShowFps => {
                m.separator();
                let item = ui::Item::new("⏱", t!("call.show_fps")).checked(overlay.fps);
                if m.add(item).clicked() {
                    acts.push(Act::ToggleFps);
                }
            }
            Row::NerdStats => {
                let item = ui::Item::new("📊", t!("call.stats_for_nerds")).checked(overlay.nerd);
                if m.add(item).clicked() {
                    acts.push(Act::ToggleNerdStats);
                }
            }
            _ => {}
        }
    }
    picked
}

/// While asking for or having control of a screen: an outline, and what is going on.
fn remote_tag(ui: &Ui, rect: Rect, entry: &Entry, on: bool, focused: bool, sharer: &str) {
    let accent = ui::accent(ui).base;
    if on {
        let width = if focused { 3.0 } else { 2.0 };
        ui.painter().rect_stroke(
            rect,
            ui::RADIUS,
            Stroke::new(width, accent),
            StrokeKind::Inside,
        );
    }
    let text = match (on, focused) {
        (false, _) => t!("call.asking_control", name = sharer),
        (true, true) => t!("call.controlling", name = entry.name),
        (true, false) => t!("call.click_to_control", name = entry.name),
    };
    let room = rect.width() - 2.0 * GAP - 16.0;
    if room < ui::TEXT_SMALL || rect.height() < 2.0 * TOOL.y + 2.0 * GAP {
        return;
    }
    let mut job =
        LayoutJob::simple_singleline(text, FontId::proportional(ui::TEXT_SMALL), ui::ON_FILL);
    job.wrap = TextWrapping::truncate_at_width(room);
    let painter = ui.painter();
    let galley = painter.layout_job(job);
    let pad = vec2(8.0, 3.0);
    // Just above the name tag, which has the same height, clear of the hover tools.
    let size = galley.size() + 2.0 * pad;
    let at = pos2(rect.left() + GAP, rect.bottom() - GAP - 2.0 * size.y - 4.0);
    let tag = Rect::from_min_size(at, size);
    let fill = if on { accent } else { ui::OVERLAY_BUTTON };
    painter.rect_filled(tag, ui::RADIUS_S, fill);
    painter.galley(tag.min + pad, galley, ui::ON_FILL);
}

/// Where a tile without video shows the person's picture: centred, or at the left
/// of a low tile.
fn avatar_spot(rect: Rect) -> Rect {
    if rect.height() < COMPACT {
        let size = (rect.height() - 2.0 * GAP).clamp(16.0, 28.0);
        let center = pos2(rect.left() + GAP + size / 2.0, rect.center().y);
        Rect::from_center_size(center, Vec2::splat(size))
    } else {
        let size = (rect.height() * 0.42)
            .min(rect.width() * 0.3)
            .clamp(20.0, 96.0);
        Rect::from_center_size(rect.center(), Vec2::splat(size))
    }
}

/// The name at the bottom left on a dark tag, with mute and deafen badges and LIVE.
fn name_tag(ui: &mut Ui, rect: Rect, entry: &Entry, audio: &crate::audio::Audio) {
    let c = ui::colors(ui);
    let person = entry.peer.person;
    let is_person = matches!(entry.tile, Tile::Person(_));
    let status = match () {
        _ if !is_person => VoiceStatus::default(),
        _ if entry.me => VoiceStatus {
            muted: audio.muted() && !audio.deafened(),
            deafened: audio.deafened(),
        },
        _ => entry.peer.status,
    };
    let local_mute = is_person && !entry.me && audio.stream_muted(&(person, None));
    let badges = sound_badges(status).chain(local_mute.then_some(("🔊", t!("call.muted_by_you"))));
    let badges_width = BADGE * badges.clone().count() as f32;
    let text = if entry.peer.connecting && is_person {
        t!("call.name_connecting", name = entry.name)
    } else {
        entry.name.clone()
    };
    let pad = vec2(8.0, 3.0);
    // Beside the picture on a low tile without video, else at the bottom left.
    let beside = (is_person && entry.picture.is_none() && rect.height() < COMPACT)
        .then(|| avatar_spot(rect).right() + GAP)
        .filter(|left| rect.right() - GAP - left - 2.0 * pad.x - badges_width >= ui::TEXT_SMALL);
    let left = beside.unwrap_or(rect.left() + GAP);
    let room = rect.right() - GAP - left - 2.0 * pad.x - badges_width;
    if room < 0.0 {
        return;
    }
    let mut job =
        LayoutJob::simple_singleline(text, FontId::proportional(ui::TEXT_SMALL), ui::OVERLAY_TEXT);
    job.wrap = TextWrapping::truncate_at_width(room);
    let painter = ui.painter();
    let galley = painter.layout_job(job);
    let height = galley.size().y + 2.0 * pad.y;
    let width = galley.size().x + 2.0 * pad.x + badges_width;
    let top = match beside {
        Some(_) => rect.center().y - height / 2.0,
        None => rect.bottom() - GAP - height,
    };
    let tag = Rect::from_min_size(pos2(left, top), vec2(width, height));
    painter.rect_filled(tag, ui::RADIUS_S, ui::OVERLAY_BUTTON);
    let text_at = tag.min + pad;
    painter.galley(text_at, galley, ui::OVERLAY_TEXT);
    for (index, (glyph, tip)) in badges.enumerate() {
        let spot = Rect::from_center_size(
            pos2(
                tag.right() - badges_width + BADGE * (index as f32 + 0.5) - 2.0,
                tag.center().y,
            ),
            Vec2::splat(BADGE - 4.0),
        );
        painter.text(
            spot.center(),
            Align2::CENTER_CENTER,
            glyph,
            FontId::proportional(ui::TEXT_SMALL),
            ui::OVERLAY_TEXT,
        );
        let half = Vec2::splat(5.0);
        painter.line_segment(
            [spot.center() - half, spot.center() + half],
            Stroke::new(2.0, c.danger_text),
        );
        let response = ui
            .interact(
                spot,
                UiId::unique(("tile badge", person, tip)),
                Sense::hover(),
            )
            .on_hover_text(tip);
        response.widget_info(|| WidgetInfo::labeled(Role::Image, true, tip));
    }
    let live = is_person && !entry.peer.screens.is_empty();
    if live && rect.height() > 2.0 * height + GAP {
        let galley = ui.painter().layout_no_wrap(
            t!("call.live_badge").into(),
            FontId::new(ui::TEXT_CAPTION - 1.0, ui::bold_family()),
            ui::ON_FILL,
        );
        let spot =
            Rect::from_min_size(rect.min + Vec2::splat(GAP), galley.size() + vec2(10.0, 2.0));
        ui.painter().rect_filled(spot, ui::RADIUS_S, c.danger);
        ui.painter()
            .galley(spot.min + vec2(5.0, 1.0), galley, ui::ON_FILL);
    }
}

/// The hover buttons at the top right of a tile: focus, minimize, full screen,
/// stop watching and volume, as many as fit.
fn tools(
    ui: &mut Ui,
    rect: Rect,
    entry: &Entry,
    state: TileState,
    audio: &crate::audio::Audio,
    acts: &mut Vec<Act>,
) {
    let person = entry.peer.person;
    let is_person = matches!(entry.tile, Tile::Person(_));
    // Kept while the volume menu is open, which the pointer leaves the tile for.
    let menu_id = UiId::unique(("tile volume open", &entry.tile));
    let menu_open = ui.data(|data| data.get_temp::<bool>(menu_id).unwrap_or(false));
    if !ui.rect_contains_pointer(rect) && !menu_open {
        return;
    }
    let fits = ((rect.width() - 2.0 * GAP) / (TOOL.x + 4.0)).floor() as usize;
    if rect.height() < TOOL.y + 2.0 * GAP || fits == 0 {
        return;
    }
    // A person's voice and apps, or the sound of a screen.
    let sound = is_person || !entry.peer.apps.is_empty();
    let volume = sound && !entry.me && !entry.peer.unknown && state.joined;
    let full = entry.picture.is_some();
    let preview = entry.tile == Tile::Preview;
    let stop = entry.watched || preview;
    // You need to see a screen to control it.
    let remote = !is_person && !entry.me && state.joined && entry.watched && full;
    // In order of importance; the first ones win on a small tile.
    let wanted = [true, is_person, full, remote, volume, stop];
    let count = wanted.iter().filter(|on| **on).count().min(fits);
    let width = count as f32 * (TOOL.x + 4.0);
    let bar = Rect::from_min_size(
        pos2(rect.right() - GAP - width, rect.top() + GAP),
        vec2(width, TOOL.y),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(bar)
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.spacing_mut().item_spacing.x = 4.0;
    let ui = &mut child;
    let mut left = count;
    let mut room = || {
        let ok = left > 0;
        left = left.saturating_sub(1);
        ok
    };
    if room() {
        let (glyph, tip) = if state.focused {
            ("🗗", t!("call.unfocus"))
        } else {
            ("🗖", t!("call.focus"))
        };
        if ui::icon_button(ui, glyph, tip, Look::Plain, TOOL).clicked() {
            acts.push(if state.focused {
                Act::Unfocus
            } else {
                Act::Focus(entry.tile.clone())
            });
        }
    }
    if is_person
        && room()
        && ui::icon_button(ui, "🗕", t!("call.minimize"), Look::Plain, TOOL).clicked()
    {
        acts.push(Act::Minimize(person));
    }
    if full
        && room()
        && ui::icon_button(ui, "⛶", t!("call.full_screen"), Look::Plain, TOOL).clicked()
    {
        acts.push(Act::Full(entry.tile.clone()));
    }
    if remote && room() {
        let on = Look::On(ui::accent(ui).base);
        let (tip, look) = match state.remote {
            None => (t!("call.request_control"), Look::Plain),
            Some(false) => (t!("call.cancel_request"), on),
            Some(true) => (t!("call.stop_control"), on),
        };
        if ui::icon_button(ui, "🖱", tip, look, TOOL).clicked() {
            acts.push(match (&entry.tile, state.remote) {
                (Tile::Screen(sid), None) => Act::RequestControl(sid.clone()),
                _ => Act::ReleaseControl,
            });
        }
    }
    if volume && room() {
        let button = ui::icon_button(ui, "🔊", t!("call.volume"), Look::Plain, TOOL);
        ui::menu(&button, |m| {
            mixer::show(m, audio, person, is_person, &entry.peer.apps)
        });
        let open = ui::menu_open(&button);
        ui.data_mut(|data| data.insert_temp(menu_id, open));
    }
    let (tip, act) = if preview {
        (t!("call.hide_preview"), Act::HidePreview)
    } else {
        (t!("call.stop_watching"), Act::StopWatching)
    };
    if stop && room() && ui::icon_button(ui, "⏹", tip, Look::Plain, TOOL).clicked() {
        acts.push(act);
    }
}

fn apply(home: &mut Home, index: usize, now: f64, acts: Vec<Act>) {
    for act in acts {
        let stage = &mut home.voice.stage;
        match act {
            Act::Focus(tile) => {
                stage.presenting = false;
                stage.focus = Some(tile);
            }
            Act::Unfocus => stage.focus = None,
            Act::Minimize(person) => {
                if let Some(at) = stage.minimized.iter().position(|p| *p == person) {
                    stage.minimized.remove(at);
                } else {
                    stage.minimized.push(person);
                    if stage.focus == Some(Tile::Person(person)) {
                        stage.focus = None;
                    }
                }
            }
            Act::Full(tile) => stage.full = Some(tile),
            Act::Watch(screen) => watch(home, &screen),
            Act::StopWatching => {
                stage.stop_watching();
                stop_watching(home);
            }
            Act::HidePreview => home.voice.stop_preview(),
            Act::ToggleFps => home.layout.split.show_fps ^= true,
            Act::ToggleNerdStats => home.layout.split.nerd_stats ^= true,
            Act::Person(person, picked) => person::apply(home, index, person, picked, now),
            Act::RequestControl(sid) => send(home, Command::RequestControl(sid)),
            Act::ReleaseControl => send(home, Command::ReleaseControl),
            Act::Control(inputs) => send(home, Command::ControlInput(inputs)),
        }
    }
}

fn send(home: &Home, command: Command) {
    if let Some(server) = call_server(home) {
        server.send(command);
    }
}

/// A camera or screen covering the whole window, with its name and t!("call.exit_full_screen").
pub fn full_view(ui: &mut Ui, home: &mut Home) {
    let Some(tile) = home.voice.stage.full.clone() else {
        return;
    };
    let ctx = ui.ctx().clone();
    let found = super::call_server(home)
        .zip(home.voice.call.as_ref())
        .and_then(|(server, call)| {
            let peers = server.voice.get(&call.channel)?;
            let me = server.dir.me;
            match &tile {
                Tile::Person(person) => {
                    let peer = peers.iter().find(|p| p.person == *person)?.clone();
                    Some((peer.name(server), Some(peer), *person == me))
                }
                Tile::Screen(sid) => {
                    let peer = peers
                        .iter()
                        .find(|p| p.screens.iter().any(|s| s.sid == *sid))?;
                    Some((
                        t!("call.someones_screen", name = peer.name(server)),
                        None,
                        false,
                    ))
                }
                Tile::Preview => previewing(&home.voice)
                    .then(|| (t!("call.your_screen_preview").to_owned(), None, true)),
            }
        });
    let Some((name, peer, me)) = found else {
        // It ended or the call did.
        home.voice.stage.full = None;
        return;
    };
    let split = home.layout.split;
    let stats = home.voice.call.as_ref().map(|call| &call.stats);
    let overlay = Overlay::new(&tile, split.show_fps, split.nerd_stats, stats);
    let voice = &mut home.voice;
    let picture = match (&tile, &peer) {
        (Tile::Person(_), Some(peer)) => person_picture(&ctx, voice, peer, me),
        (Tile::Screen(sid), _) => screen_picture(&ctx, voice, sid),
        (Tile::Preview, _) => preview_picture(&ctx, voice),
        _ => None,
    };
    // Camera turned off or hidden: nothing left to show big.
    if peer.as_ref().is_some_and(|p| p.camera.is_none()) {
        voice.stage.full = None;
        return;
    }
    let controlling = match &tile {
        Tile::Screen(sid) => voice.control.screen(sid) == Some(true),
        Tile::Person(_) | Tile::Preview => false,
    };
    // While controlling, Escape belongs to the shared screen.
    let mut exit = !controlling && ui.input(|input| input.key_pressed(Key::Escape));
    let heading = if controlling {
        t!("call.controlling", name = name)
    } else {
        name
    };
    ui.horizontal(|ui| {
        ui.label(ui::bold(&heading));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            exit |= ui::secondary(ui, t!("call.exit_full_screen")).clicked();
        });
    });
    ui.add_space(ui::SPACE_S);
    let sense = if controlling {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), sense);
    match picture {
        Some(mut picture) => {
            // Whole, not cropped, now that it has the room.
            picture.cover = false;
            paint_picture(ui, rect, picture);
            overlay.paint(ui, rect);
            if controlling {
                let (inputs, stop) =
                    voice
                        .control
                        .capture(ui, &response, fitted(rect, picture.size));
                let accent = ui::accent(ui).base;
                ui.painter().rect_stroke(
                    fitted(rect, picture.size),
                    0.0,
                    Stroke::new(2.0, accent),
                    StrokeKind::Outside,
                );
                if !inputs.is_empty() {
                    send(home, Command::ControlInput(inputs));
                }
                if stop {
                    send(home, Command::ReleaseControl);
                }
                if exit {
                    home.voice.stage.full = None;
                }
                return;
            }
        }
        None => {
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                t!("call.waiting_picture"),
                FontId::proportional(ui::TEXT_BODY),
                ui::colors(ui).muted,
            );
        }
    }
    if exit {
        voice.stage.full = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), vec2(w, h))
    }

    fn inside(outer: Rect, inner: Rect) -> bool {
        outer.expand(0.01).contains_rect(inner)
    }

    fn apart(rects: &[Rect]) -> bool {
        rects.iter().enumerate().all(|(i, a)| {
            rects[i + 1..]
                .iter()
                .all(|b| !a.shrink(0.01).intersects(b.shrink(0.01)))
        })
    }

    #[test]
    fn stopping_a_screen_leaves_no_big_screen_to_receive() {
        let mut stage = Stage {
            focus: Some(Tile::Screen("s".into())),
            ..Stage::default()
        };
        stage.stop_watching();
        assert_eq!(stage.focus, None);
        // A focused person stays focused.
        stage.focus = Some(Tile::Person([0; 32]));
        stage.presenting = true;
        stage.stop_watching();
        assert!(stage.focus.is_some() && !stage.presenting);
    }

    #[test]
    fn grid_fits_the_count_and_the_stage() {
        assert!(grid(0, area(800.0, 450.0), GAP).is_empty());
        // One tile fills a 16:9 stage.
        let one = grid(1, area(800.0, 450.0), GAP);
        assert_eq!(one[0], area(800.0, 450.0));
        // Four in a wide stage: two by two.
        let stage = area(1000.0, 600.0);
        let four = grid(4, stage, GAP);
        assert_eq!(four[0].top(), four[1].top());
        assert_eq!(four[0].left(), four[2].left());
        assert!(four.iter().all(|r| inside(stage, *r)) && apart(&four));
        // A tall, narrow stage stacks them.
        let tall = grid(3, area(300.0, 900.0), GAP);
        assert!(tall.iter().all(|r| r.left() == tall[0].left()));
        // Three in a wide stage: the short last row is centred.
        let three = grid(3, stage, GAP);
        assert_eq!(three[2].center().x, stage.center().x);
        for count in 1..=12 {
            let tiles = grid(count, stage, GAP);
            assert_eq!(tiles.len(), count);
            assert!(tiles.iter().all(|r| inside(stage, *r)), "{count}");
            assert!(apart(&tiles), "{count}");
            assert!(
                tiles
                    .iter()
                    .all(|r| (r.aspect_ratio() - ASPECT).abs() < 0.01)
            );
        }
    }

    #[test]
    fn spotlight_puts_the_others_in_a_strip() {
        let wide = area(1000.0, 600.0);
        let (big, strip) = spotlight(0, wide, GAP, FOCUS_STRIP);
        assert_eq!(big, wide);
        assert!(strip.is_empty());
        let (big, strip) = spotlight(3, wide, GAP, FOCUS_STRIP);
        assert_eq!(strip.len(), 3);
        let thick = 600.0 * FOCUS_STRIP;
        assert_eq!(big.bottom(), 600.0 - thick - GAP);
        assert!(strip.iter().all(|r| r.top() >= big.bottom() + GAP));
        assert!(strip.iter().all(|r| inside(wide, *r)) && apart(&strip));
        // A tall stage keeps the strip at the bottom, where wide tiles fit better.
        let tall = area(400.0, 900.0);
        let (big, strip) = spotlight(2, tall, GAP, FOCUS_STRIP);
        assert!(strip.iter().all(|r| r.top() >= big.bottom() + GAP));
        assert!(strip.iter().all(|r| inside(tall, *r)) && apart(&strip));
        // A very wide one gets it on the right.
        let flat = area(1400.0, 300.0);
        let (big, strip) = spotlight(2, flat, GAP, FOCUS_STRIP);
        assert!(strip.iter().all(|r| r.left() >= big.right() + GAP));
        assert!(strip.iter().all(|r| inside(flat, *r)) && apart(&strip));
        // Many tiles shrink to fit the strip instead of running off it.
        let (_, strip) = spotlight(30, wide, GAP, PRESENT_STRIP);
        assert!(strip.iter().all(|r| inside(wide, *r)) && apart(&strip));
        // The strip stays usable on tiny and huge stages.
        let (big, _) = spotlight(1, area(2000.0, 2000.0), GAP, FOCUS_STRIP);
        assert_eq!(big.bottom(), 2000.0 - STRIP_RANGE.end() - GAP);
        let (big, _) = spotlight(1, area(200.0, 100.0), GAP, FOCUS_STRIP);
        assert_eq!(big.bottom(), 100.0 - STRIP_RANGE.start() - GAP);
    }

    #[test]
    fn bubbles_line_up_from_the_bottom_left() {
        let stage = area(200.0, 300.0);
        let first = bubble(0, stage, GAP);
        assert_eq!(first.left_bottom(), pos2(GAP, 300.0 - GAP));
        assert_eq!(first.width(), BUBBLE);
        let second = bubble(1, stage, GAP);
        assert_eq!(second.left(), first.right() + GAP);
        // Four fit on a row of 200; the fifth starts a row above.
        let per_row = bubbles_per_row(stage, GAP);
        assert_eq!(per_row, 4);
        let fifth = bubble(4, stage, GAP);
        assert_eq!(fifth.left(), first.left());
        assert_eq!(fifth.bottom(), first.top() - GAP);
        assert_eq!(bubbles_height(4, stage, GAP), BUBBLE + GAP);
        assert_eq!(bubbles_height(5, stage, GAP), 2.0 * (BUBBLE + GAP));
        // Never zero per row, even on a stage narrower than one bubble.
        assert_eq!(bubble(1, area(20.0, 300.0), GAP).left(), GAP);
    }

    #[test]
    fn cover_crops_the_long_side() {
        let uv = cover_uv(vec2(1600.0, 900.0), vec2(100.0, 100.0));
        assert!((uv.width() - 900.0 / 1600.0).abs() < 1e-5);
        assert_eq!(uv.height(), 1.0);
        assert!((uv.center().x - 0.5).abs() < 1e-5);
        let uv = cover_uv(vec2(900.0, 1600.0), vec2(160.0, 90.0));
        assert_eq!(uv.width(), 1.0);
        assert!(uv.height() < 1.0);
    }
}
