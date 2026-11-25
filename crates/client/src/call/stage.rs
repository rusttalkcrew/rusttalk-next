//! The call stage in a voice channel: a tile per person and shared terminal, focus,
//! minimized bubbles, t!("call.show_only_video"), presentation mode and full screen.

use std::ops::RangeInclusive;

use eframe::egui::{
    self, Align, Align2, CursorIcon, FontId, Id as UiId, Image, Key, Layout, Popup, Rect, Role,
    Sense, Stroke, StrokeKind, TextureId, Ui, UiBuilder, Vec2, WidgetInfo, pos2,
    text::{LayoutJob, TextWrapping},
    vec2,
};
use rusttalk_core::client::voice::{ControlOffer, VoiceStatus};

use super::{
    Sharing, Voice, call_server, join, mixer,
    overlay::{self, Overlay, Row},
    pip::{self, Pip},
    remote::{self, Control},
    sound_badges, stop_watching,
    terminal::{self, Terminals, Typing},
    upload, watch,
};
use crate::{
    friends::Relation,
    home::{
        Home, face_speaking,
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

/// What a tile shows: a person (their camera or picture), a shared screen, the
/// preview of our own, or a person's shared terminal.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Tile {
    Person(Id),
    Screen(String),
    Preview,
    Terminal(Id),
}

impl Tile {
    /// Shared rather than a person: what presentation mode shows big.
    fn shared(&self) -> bool {
        matches!(self, Tile::Screen(_) | Tile::Preview | Tile::Terminal(_))
    }
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
    /// When full screen shows its controls.
    chrome: Chrome,
}

/// Seconds the full-screen controls stay after the pointer last moved.
const IDLE: f64 = 2.5;

/// When full screen shows its controls, like a video player: on entering, when the
/// pointer moves and while they are in use; gone after `IDLE` quiet seconds. A tap
/// (phones) shows or hides them.
#[derive(Default)]
struct Chrome {
    until: Option<f64>,
    /// Where the controls were last drawn; the pointer resting there keeps them.
    panels: Vec<Rect>,
}

impl Chrome {
    /// Whether the controls show now. `held`: one of their menus or dialogs is open.
    fn update(&mut self, now: f64, moved: bool, tapped: bool, held: bool) -> bool {
        let shown = now < *self.until.get_or_insert(now + IDLE);
        if held || moved || (tapped && !shown) {
            self.until = Some(now + IDLE);
            true
        } else if tapped {
            self.until = Some(now);
            false
        } else {
            shown
        }
    }

    fn over(&self, pointer: Option<egui::Pos2>) -> bool {
        pointer.is_some_and(|at| self.panels.iter().any(|panel| panel.contains(at)))
    }

    /// Seconds until the controls hide.
    fn left(&self, now: f64) -> f64 {
        self.until.map_or(IDLE, |until| until - now).max(0.0)
    }
}

impl Stage {
    #[cfg(test)]
    pub(crate) fn set_presenting_and_full_screen(&mut self, sid: &str) {
        self.presenting = true;
        self.full = Some(Tile::Screen(sid.to_owned()));
    }

    #[cfg(test)]
    pub(crate) fn set_full_screen_preview(&mut self) {
        self.full = Some(Tile::Preview);
    }

    #[cfg(test)]
    pub(crate) fn is_presenting(&self) -> bool {
        self.presenting
    }

    pub(super) fn replace_video(&mut self, old: &str, new: &str) {
        for tile in [&mut self.focus, &mut self.full] {
            if let Some(Tile::Screen(sid)) = tile
                && sid == old
            {
                *sid = new.to_owned();
            }
        }
    }

    /// Stopping a screen also drops its big tile: a big screen is received on its
    /// own (see `stage`), so keeping it big would start watching again at once.
    fn stop_watching(&mut self) {
        self.presenting = false;
        if matches!(self.focus, Some(Tile::Screen(_))) {
            self.focus = None;
        }
        if matches!(self.full, Some(Tile::Screen(_))) {
            self.full = None;
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
    if !voice.video.preview.is_on() {
        return None;
    }
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
    matches!(voice.sharing, Sharing::On | Sharing::Changing)
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
        let mirrored = voice.camera.mirrored();
        let texture = voice.camera.self_texture(ctx)?;
        return Some(Picture {
            texture: texture.id(),
            size: texture.size_vec2(),
            mirrored,
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
    camera_visible: bool,
    speaking: bool,
    /// The sharer's camera, on top of their screen.
    camera: Option<OnScreen>,
    /// A camera tile popped out of its sharer's screen.
    popped: bool,
}

/// A camera shown on its sharer's screen.
#[derive(Clone, Copy)]
struct OnScreen {
    pip: Pip,
    /// None while minimized, or until the first picture arrives.
    picture: Option<Picture>,
}

/// What a tile asked for this frame.
enum Act {
    Listen(String, bool),
    Focus(Tile),
    Unfocus,
    Minimize(Id),
    Full(Tile),
    Watch(Screen),
    StopWatching,
    ShowPreview,
    ToggleFps,
    ToggleNerdStats,
    /// Picked on someone's card or in their menu.
    Person(Id, Pick),
    RequestControl(String),
    ReleaseControl,
    /// Input for the screen we control.
    Control(Vec<Input>),
    /// Watch the terminal this device shares.
    WatchTerminal(String),
    StopWatchingTerminal,
    /// Ask to type in the terminal we watch, or stop asking or typing.
    AskToType,
    TypeInTerminal(Vec<u8>),
    PasteRefused,
    /// Moved, popped out, put back, minimized or restored a camera on a screen.
    Pip(Id, pip::Change),
}

fn selected_screen<'a>(peer: &'a Peer, watched: Option<&str>) -> Option<&'a Screen> {
    peer.screens
        .iter()
        .find(|s| Some(s.sid.as_str()) == watched)
        .or_else(|| peer.screens.iter().find(|s| s.local))
        .or_else(|| peer.screens.first())
}

/// One card per person; terminals retain their separate interactive surface.
fn entries<'p>(
    ctx: &egui::Context,
    voice: &mut Voice,
    server: &Server,
    channel: &Id,
    peers: &'p [Peer],
    joined: bool,
) -> Vec<Entry<'p>> {
    let mut screens = Vec::new();
    let mut people = Vec::new();
    for peer in peers {
        let name = peer.name(server);
        let me = peer.person == server.dir.me;
        let speaking = voice.speaking(server, channel, &peer.person);
        let camera = joined && peer.camera.is_some();
        let video = camera && (me || !voice.hidden.contains(&peer.person));
        let sharing = if me {
            voice.terminals.local.is_some()
        } else {
            peer.terminal.is_some()
        };
        if joined && sharing {
            let watched = !me && voice.terminals.watched() == peer.terminal.as_deref();
            screens.push(Entry {
                tile: Tile::Terminal(peer.person),
                peer,
                name: if me {
                    t!("call.your_terminal").to_owned()
                } else {
                    t!("call.someones_terminal", name = name)
                },
                me,
                picture: None,
                video: true,
                watched,
                camera_visible: video,
                speaking: false,
                camera: None,
                popped: false,
            });
        }
        let screen = joined
            .then(|| selected_screen(peer, voice.watching.as_ref().map(|w| w.sid.as_str())))
            .flatten();
        let camera_visible = video;
        let (tile, picture, video, watched) = match screen {
            Some(screen) if screen.local && previewing(voice) => {
                (Tile::Preview, preview_picture(ctx, voice), true, false)
            }
            Some(screen) if !screen.local => {
                let watched = voice.watching.as_ref().is_some_and(|w| w.sid == screen.sid);
                (
                    Tile::Screen(screen.sid.clone()),
                    screen_picture(ctx, voice, &screen.sid),
                    true,
                    watched,
                )
            }
            _ => {
                let picture = video
                    .then(|| person_picture(ctx, voice, peer, me))
                    .flatten();
                (Tile::Person(peer.person), picture, video, false)
            }
        };
        // A screen covers its sharer's tile, so their camera goes on top of it, or
        // beside it when popped out. Minimized, it is neither received nor drawn.
        let on_screen = matches!(tile, Tile::Screen(_) | Tile::Preview) && camera_visible;
        let pip = voice.pips.get(&peer.person).copied().unwrap_or_default();
        let camera = (on_screen && !pip.popped).then(|| OnScreen {
            pip,
            picture: (!pip.minimized)
                .then(|| person_picture(ctx, voice, peer, me))
                .flatten(),
        });
        let popped = (on_screen && pip.popped).then(|| Entry {
            tile: Tile::Person(peer.person),
            peer,
            name: name.clone(),
            me,
            picture: person_picture(ctx, voice, peer, me),
            video: true,
            watched: false,
            camera_visible,
            speaking,
            camera: None,
            popped: true,
        });
        people.push(Entry {
            tile,
            peer,
            name,
            me,
            picture,
            video,
            watched,
            camera_visible,
            speaking,
            camera,
            popped: false,
        });
        people.extend(popped);
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
    let calling = crate::home::voice_requests::calling(&home.voice, server);
    let peers = server.voice.get(&channel).cloned().unwrap_or_default();
    let ctx = ui.ctx().clone();
    let voice = &mut home.voice;
    let video_only = &mut home.layout.split.video_only;
    let all = entries(&ctx, voice, server, &channel, &peers, joined);
    let retaining = voice
        .watching
        .as_ref()
        .is_some_and(|w| voice.retaining_watch(&w.sid));
    let stage = &mut voice.stage;
    stage
        .minimized
        .retain(|person| peers.iter().any(|p| p.person == *person));
    let presentable = all.iter().position(|e| e.tile.shared());
    if presentable.is_none() && !retaining {
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
            Tile::Screen(_) | Tile::Preview | Tile::Terminal(_) => true,
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
        watched.or_else(|| shown.iter().position(|e| e.tile.shared()))
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
    // So is a big terminal.
    if let Some(entry) = big.map(|at| shown[at])
        && let Tile::Terminal(_) = entry.tile
        && !entry.watched
        && !entry.me
        && joined
        && let Some(sharer) = &entry.peer.terminal
    {
        acts.push(Act::WatchTerminal(sharer.clone()));
    }
    if !stage.presenting
        && big.is_none()
        && !matches!(&stage.focus, Some(Tile::Screen(sid)) if voice.watching.as_ref().is_some_and(|watch| watch.sid == *sid))
    {
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
        } else if !bubbles.is_empty() {
            // Everyone left is minimized: not a connection problem.
            t!("call.all_minimized")
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
    let voice = &mut home.voice;
    let stats = voice.call.as_ref().map(|call| &call.stats);
    let (stage, control, terms) = (&voice.stage, &mut voice.control, &mut voice.terminals);
    for (at, (entry, target)) in shown.iter().zip(&targets).enumerate() {
        let rect = animate(&ctx, &entry.tile, *target, rect);
        let is_big = big == Some(at);
        let focused = stage.focus.as_ref() == Some(&entry.tile);
        let remote = match &entry.tile {
            Tile::Screen(sid) if joined && !entry.me => control.screen(sid),
            _ => None,
        };
        let typing = match entry.tile {
            Tile::Terminal(_) if entry.watched => terms.typing,
            _ => Typing::Idle,
        };
        let state = TileState {
            relation: home.friends.relation(&entry.peer.person),
            can_ask: calling && !joined,
            joined,
            big: is_big,
            spotlight: big.is_some(),
            focused,
            remote,
            typing,
        };
        let overlay = Overlay::new(&entry.tile, split.show_fps, split.nerd_stats, stats);
        let tools = (&mut *control, &mut *terms);
        tile(
            ui, rect, entry, state, server, &audio, tools, &overlay, &mut acts,
        );
    }
    for (at, entry) in bubbles.iter().enumerate() {
        let spot = bubble(at, rect, GAP);
        let mut child = ui.new_child(UiBuilder::new().max_rect(spot));
        face_speaking(
            &mut child,
            &server.dir,
            &entry.peer.person,
            &entry.name,
            BUBBLE,
            false,
            entry.speaking,
        );
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
    /// This device is in a call in another channel here, and can ask them over.
    can_ask: bool,
    joined: bool,
    /// The focused or presented tile.
    big: bool,
    /// Someone is big, so the others are in the strip.
    spotlight: bool,
    focused: bool,
    /// This screen is ours to control (`true`), or we asked to.
    remote: Option<bool>,
    /// Typing in this terminal we watch.
    typing: Typing,
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
    (control, terms): (&mut Control, &mut Terminals),
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
            let (dir, name) = (&server.dir, &entry.name);
            face_speaking(&mut child, dir, &person, name, size, false, entry.speaking);
        }
        // Drawn below, above the tile's own clicks: selecting text takes the pointer.
        (None, Tile::Terminal(_)) if entry.me || entry.watched => {}
        (None, Tile::Screen(_) | Tile::Preview | Tile::Terminal(_)) => {
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
    let (tapped, reveal) = ui::touch_tools(&response);
    let screen = match &entry.tile {
        Tile::Screen(sid) => entry.peer.screens.iter().find(|s| s.sid == *sid),
        Tile::Person(_) | Tile::Preview | Tile::Terminal(_) => None,
    };
    let watch_click = screen.filter(|_| !entry.watched && state.joined);
    let terminal_click = match &entry.tile {
        Tile::Terminal(_) if !entry.watched && !entry.me && state.joined => {
            entry.peer.terminal.as_ref()
        }
        _ => None,
    };
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
        if response.clicked() && !reveal {
            acts.push(Act::Focus(entry.tile.clone()));
        }
    } else if let Some(screen) = watch_click {
        let label = t!("call.watch_name", name = entry.name);
        response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
        if response.clicked() && !reveal {
            acts.push(Act::Watch(screen.clone()));
        }
    } else if let Some(sharer) = terminal_click {
        let label = t!("call.watch_name", name = entry.name);
        response.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
        if response.clicked() && !reveal {
            acts.push(Act::WatchTerminal(sharer.clone()));
        }
    } else {
        let role = match entry.tile {
            Tile::Person(_) => Role::Button,
            Tile::Screen(_) | Tile::Preview | Tile::Terminal(_) => Role::Group,
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
            can_ask: state.can_ask,
            audio: others.then_some(audio),
            video: (others && entry.peer.camera.is_some()).then_some(entry.video),
        };
        // In the strip a click focuses the tile, so only the menu is there.
        let picked = if !entry.peer.screens.is_empty() || reveal || (state.spotlight && !state.big)
        {
            ui::context_menu(&response, |m| {
                screen_choices(m, entry, acts);
                target.menu(m)
            })
            .flatten()
        } else {
            person::interact(ui, &response, &target, true)
        };
        if let Some(picked) = picked {
            acts.push(Act::Person(person, picked));
        }
    }
    // A terminal has no video to measure, and its own tile takes the pointer.
    if matches!(entry.tile, Tile::Screen(_) | Tile::Preview) && !controlling {
        // The sharer's own menu sits inside, for someone else's screen.
        let sharer = (state.joined && !entry.peer.unknown).then_some(Person {
            server,
            person,
            relation: state.relation,
            can_ask: false,
            audio: (!entry.me).then_some(audio),
            video: (!entry.me && entry.peer.camera.is_some()).then_some(entry.camera_visible),
        });
        let menu = ui::context_menu(&response, |m| {
            screen_menu(
                m,
                entry,
                state,
                audio,
                sharer.as_ref(),
                overlay,
                false,
                acts,
            )
        });
        if let Some(Some(picked)) = menu {
            acts.push(Act::Person(person, picked));
        }
    }
    if let Tile::Terminal(_) = entry.tile {
        // In the strip a click focuses the tile instead.
        let active = !state.spotlight || state.big;
        if entry.me
            && let Some(local) = &mut terms.local
        {
            let guests = (&terms.guests, Some(server));
            terminal::ours(ui, rect, local, guests, active, &entry.name);
        } else if entry.watched {
            let typing = state.typing == Typing::On;
            let screen = terms
                .watching
                .as_mut()
                .and_then(|(_, screen)| screen.as_mut());
            let (typed, refused) =
                terminal::theirs(ui, rect, screen, (active, typing), &entry.name);
            if !typed.is_empty() {
                acts.push(Act::TypeInTerminal(typed));
            }
            if refused {
                acts.push(Act::PasteRefused);
            }
        }
    }
    if let Some(on) = state.remote {
        let sharer = entry.peer.name(server);
        remote_tag(ui, rect, entry, on, response.has_focus(), &sharer);
    }
    let now = ui.input(|input| input.time);
    if let (None, Tile::Screen(sid)) = (state.remote, &entry.tile)
        && let Some(text) = remote::note(control, sid, now)
    {
        note_tag(ui, rect, text);
    }
    let badge = name_tag(ui, rect, entry, audio);
    overlay.paint(ui, rect, badge);
    tools(ui, rect, entry, state, audio, acts, tapped);
    if let Some(camera) = entry.camera {
        // Clear of the hover buttons at the top and the name at the bottom.
        let tools = if ui.input(|i| i.has_touch_screen()) {
            ui::TOUCH_TARGET
        } else {
            TOOL.y
        };
        let mut clear = rect;
        clear.min.y += GAP + tools;
        clear.max.y -= GAP + BADGE;
        let stream = entry.picture.map_or(rect, |p| fitted(rect, p.size));
        camera_pip(ui, stream, clear, entry, camera, server, acts);
    }
}

/// The sharer's camera on their stream, in its part `clear` of the controls.
fn camera_pip(
    ui: &mut Ui,
    stream: Rect,
    clear: Rect,
    entry: &Entry,
    camera: OnScreen,
    server: &Server,
    acts: &mut Vec<Act>,
) {
    let person = entry.peer.person;
    let picture = camera.picture;
    let aspect = picture.map(|p| p.size.x / p.size.y);
    let area = stream.intersect(clear);
    let changed = pip::show(
        ui,
        stream,
        area,
        person,
        &entry.name,
        camera.pip,
        aspect,
        |ui, rect| {
            match picture {
                Some(picture) => paint_picture(ui, rect, picture),
                // Until its first picture arrives.
                None => {
                    let size = (rect.height() * 0.5).min(rect.width() * 0.5);
                    let spot = Rect::from_center_size(rect.center(), Vec2::splat(size));
                    let mut child = ui.new_child(UiBuilder::new().max_rect(spot));
                    face_speaking(
                        &mut child,
                        &server.dir,
                        &person,
                        &entry.name,
                        size,
                        false,
                        false,
                    );
                }
            }
        },
    );
    if let Some(change) = changed {
        acts.push(Act::Pip(person, change));
    }
}

fn screen_choices(m: &mut ui::Menu, entry: &Entry, acts: &mut Vec<Act>) {
    for screen in &entry.peer.screens {
        if m.item(
            "🖥",
            &t!(
                "call.watch_stream",
                name = screen.name,
                count = screen.viewers
            ),
        )
        .clicked()
        {
            acts.push(if screen.local {
                Act::ShowPreview
            } else {
                Act::Watch(screen.clone())
            });
        }
    }
}

/// The right-click menu of a screen tile; the same actions as its hover buttons, its
/// stream sound and the overlays. Returns what was picked in the sharer's submenu.
/// In full screen (`full`) it has no layout rows and keeps to the screen shown.
#[allow(clippy::too_many_arguments)]
fn screen_menu(
    m: &mut ui::Menu,
    entry: &Entry,
    state: TileState,
    audio: &crate::audio::Audio,
    sharer: Option<&Person>,
    overlay: &Overlay,
    full: bool,
    acts: &mut Vec<Act>,
) -> Option<Pick> {
    let mut picked = None;
    if !full {
        screen_choices(m, entry, acts);
    }
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
    let layout = |row: &Row| matches!(row, Row::Focus | Row::Unfocus | Row::FullScreen);
    for row in rows.into_iter().filter(|row| !(full && layout(row))) {
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
            Row::Volume => {
                m.separator();
                // The sharer's apps, whose sound arrives with the screen.
                mixer::show(m, audio, person, false, &entry.peer.apps, |sid, on| {
                    acts.push(Act::Listen(sid, on))
                });
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

/// Why asking for control of this screen ended, where the person clicked, above
/// the name tag like the "asking" tag it replaces.
fn note_tag(ui: &mut Ui, rect: Rect, text: &str) {
    let room = rect.width() - 2.0 * GAP - 16.0;
    if room < ui::TEXT_SMALL || rect.height() < 2.0 * TOOL.y + 2.0 * GAP {
        return;
    }
    let font = FontId::proportional(ui::TEXT_SMALL);
    let mut job = LayoutJob::simple(text.to_owned(), font.clone(), ui::ON_FILL, room);
    job.wrap.max_rows = 3;
    let galley = ui.painter().layout_job(job);
    let pad = vec2(8.0, 3.0);
    let size = galley.size() + 2.0 * pad;
    // The name tag below is one line of the same size.
    let line = ui.fonts_mut(|fonts| fonts.row_height(&font)) + 2.0 * pad.y;
    let at = pos2(rect.left() + GAP, rect.bottom() - GAP - line - 4.0 - size.y);
    let tag = Rect::from_min_size(at, size);
    let painter = ui.painter();
    painter.rect_filled(tag, ui::RADIUS_S, ui::colors(ui).danger);
    painter.galley(tag.min + pad, galley, ui::ON_FILL);
    let response = ui.interact(tag, UiId::unique(("control note", text)), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, true, text));
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(500));
}

/// Why a screen can't be asked for control, when its sharer said so. Unknown (an
/// older app) can be asked, as before.
fn control_closed(offer: Option<ControlOffer>) -> Option<String> {
    match offer? {
        ControlOffer::On => None,
        ControlOffer::Off => Some(t!("call.control_off").into()),
        ControlOffer::Unsupported => Some(t!("call.control_unsupported").into()),
    }
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
fn name_tag(ui: &mut Ui, rect: Rect, entry: &Entry, audio: &crate::audio::Audio) -> Option<Rect> {
    let c = ui::colors(ui);
    let person = entry.peer.person;
    let is_person = !matches!(entry.tile, Tile::Terminal(_));
    let status = match () {
        _ if !is_person => VoiceStatus::default(),
        _ if entry.me => VoiceStatus {
            muted: audio.display_muted() && !audio.deafened(),
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
    let beside = (matches!(entry.tile, Tile::Person(_))
        && entry.picture.is_none()
        && rect.height() < COMPACT)
        .then(|| avatar_spot(rect).right() + GAP)
        .filter(|left| rect.right() - GAP - left - 2.0 * pad.x - badges_width >= ui::TEXT_SMALL);
    let left = beside.unwrap_or(rect.left() + GAP);
    let room = rect.right() - GAP - left - 2.0 * pad.x - badges_width;
    if room < 0.0 {
        return None;
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
    let stream = match &entry.tile {
        Tile::Screen(sid) => entry.peer.screens.iter().find(|s| s.sid == *sid),
        Tile::Preview => entry.peer.screens.iter().find(|s| s.local),
        _ => selected_screen(entry.peer, None),
    };
    let live = is_person && (!entry.peer.screens.is_empty() || entry.peer.terminal.is_some());
    (live && rect.height() > height + 2.0 * GAP)
        .then(|| stream_badge(ui, rect, &entry.tile, stream.map(|s| s.viewers)))
}

/// A compact watcher count, with the full streaming label on hover and for screen readers.
fn stream_badge(ui: &Ui, tile: Rect, entry: &Tile, viewers: Option<usize>) -> Rect {
    let label = viewers
        .map(|count| t!("call.streaming_viewers", count = count))
        .unwrap_or_else(|| t!("call.live_badge").into());
    let at = tile.min + Vec2::splat(GAP);
    let pad = vec2(5.0, 1.0);
    let layout = |text| {
        let mut job = LayoutJob::simple_singleline(
            text,
            FontId::new(ui::TEXT_CAPTION - 1.0, crate::emoji::family(true)),
            ui::ON_FILL,
        );
        job.wrap = TextWrapping::truncate_at_width((tile.width() - 2.0 * (GAP + pad.x)).max(1.0));
        ui.painter().layout_job(job)
    };
    let id = UiId::unique(("stream status", entry));
    let mut galley = layout(viewers.map_or_else(|| label.clone(), |count| format!("👁 {count}")));
    let compact = Rect::from_min_size(at, galley.size() + 2.0 * pad);
    // The detail remains open while the pointer moves across the expanded badge.
    let hover = ui.ctx().read_response(id).map_or(compact, |last| {
        Rect::from_min_size(at, last.rect.size())
            .union(compact)
            .intersect(tile)
    });
    if viewers.is_some() && ui.rect_contains_pointer(hover) {
        galley = layout(label.clone());
    }
    let spot = Rect::from_min_size(at, galley.size() + 2.0 * pad);
    ui.interact(spot, id, Sense::hover())
        .widget_info(|| WidgetInfo::labeled(Role::Label, true, &label));
    ui.painter()
        .rect_filled(spot, ui::RADIUS_S, ui::colors(ui).danger);
    ui.painter().galley(spot.min + pad, galley, ui::ON_FILL);
    spot
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
    tapped: bool,
) {
    let person = entry.peer.person;
    let is_person = matches!(entry.tile, Tile::Person(_));
    // Kept while the volume menu is open, which the pointer leaves the tile for.
    let menu_id = UiId::unique(("tile volume open", &entry.tile));
    let menu_open = ui.data(|data| data.get_temp::<bool>(menu_id).unwrap_or(false));
    if !tapped && !menu_open && (!ui.rect_contains_pointer(rect) || ui.input(|i| i.any_touches())) {
        return;
    }
    let available = rect.size() - Vec2::splat(2.0 * GAP);
    let fits = (available.x / (TOOL.x + 4.0)).floor() as usize;
    if available.y < TOOL.y || fits == 0 {
        return;
    }
    let terminal = matches!(entry.tile, Tile::Terminal(_));
    // A person's voice and apps, or the sound of a screen.
    let sound = is_person || (!terminal && !entry.peer.apps.is_empty());
    let volume = sound && !entry.me && !entry.peer.unknown && state.joined;
    let full = entry.picture.is_some() || terminal && (entry.me || entry.watched);
    let stop = entry.watched;
    // You need to see a screen to control it.
    let remote = !is_person && !entry.me && state.joined && entry.watched && full;
    // In order of importance; the first ones win on a small tile.
    let wanted = [true, entry.popped, is_person, full, remote, volume, stop];
    let count = wanted.iter().filter(|on| **on).count().min(fits);
    // Enlarge touch targets when there is room for the same set of actions.
    let tool = if tapped {
        Vec2::splat(
            ui::TOUCH_TARGET
                .min(available.y)
                .min(available.x / count as f32 - 4.0),
        )
    } else {
        TOOL
    };
    let width = count as f32 * (tool.x + 4.0);
    let bar = Rect::from_min_size(
        pos2(rect.right() - GAP - width, rect.top() + GAP),
        vec2(width, tool.y),
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
        if ui::icon_button(ui, glyph, tip, Look::Plain, tool).clicked() {
            acts.push(if state.focused {
                Act::Unfocus
            } else {
                Act::Focus(entry.tile.clone())
            });
        }
    }
    if entry.popped
        && room()
        && ui::icon_button(ui, "↙", t!("call.camera_on_screen"), Look::Plain, tool).clicked()
    {
        acts.push(Act::Pip(person, pip::Change::PutBack));
    }
    if is_person
        && room()
        && ui::icon_button(ui, "🗕", t!("call.minimize"), Look::Plain, tool).clicked()
    {
        acts.push(Act::Minimize(person));
    }
    if full
        && room()
        && ui::icon_button(ui, "⛶", t!("call.full_screen"), Look::Plain, tool).clicked()
    {
        acts.push(Act::Full(entry.tile.clone()));
    }
    if remote && room() {
        control_button(ui, entry, state, tool, acts);
    }
    if volume && room() {
        let open = volume_button(ui, entry, audio, tool, acts);
        ui.data_mut(|data| data.insert_temp(menu_id, open));
    }
    let (tip, act) = if terminal {
        (t!("call.stop_watching"), Act::StopWatchingTerminal)
    } else {
        (t!("call.stop_watching"), Act::StopWatching)
    };
    if stop && room() && ui::icon_button(ui, "⏹", tip, Look::Plain, tool).clicked() {
        acts.push(act);
    }
}

/// Ask to type in a watched terminal, or ask for control of a watched screen; while
/// asking or in control, cancel or stop.
fn control_button(ui: &mut Ui, entry: &Entry, state: TileState, tool: Vec2, acts: &mut Vec<Act>) {
    let on = Look::On(ui::accent(ui).base);
    if let Tile::Terminal(_) = entry.tile {
        let (tip, look) = match state.typing {
            Typing::Idle => (t!("terminal.ask_to_type"), Look::Plain),
            Typing::Asking(_) => (t!("call.cancel_request"), on),
            Typing::On => (t!("terminal.stop_typing"), on),
        };
        if ui::icon_button(ui, "⌨", tip, look, tool).clicked() {
            acts.push(Act::AskToType);
        }
        return;
    }
    let (tip, look) = match state.remote {
        None => (t!("call.request_control"), Look::Plain),
        Some(false) => (t!("call.cancel_request"), on),
        Some(true) => (t!("call.stop_control"), on),
    };
    // The sharer says it won't take requests: say why instead of being refused.
    let closed = match (&entry.tile, state.remote) {
        (Tile::Screen(sid), None) => entry
            .peer
            .screens
            .iter()
            .find(|screen| screen.sid == *sid)
            .and_then(|screen| control_closed(screen.control)),
        _ => None,
    };
    let button = ui
        .add_enabled_ui(closed.is_none(), |ui| {
            ui::icon_button(ui, "🖱", tip, look, tool)
        })
        .inner;
    if let Some(why) = closed {
        button.on_disabled_hover_text(why);
    } else if button.clicked() {
        acts.push(match (&entry.tile, state.remote) {
            (Tile::Screen(sid), None) => Act::RequestControl(sid.clone()),
            _ => Act::ReleaseControl,
        });
    }
}

/// A person's voice and apps, or the sound of a screen. True while its menu is open.
fn volume_button(
    ui: &mut Ui,
    entry: &Entry,
    audio: &crate::audio::Audio,
    tool: Vec2,
    acts: &mut Vec<Act>,
) -> bool {
    let is_person = matches!(entry.tile, Tile::Person(_));
    let button = ui::icon_button(ui, "🔊", t!("call.volume"), Look::Plain, tool);
    ui::menu(&button, |m| {
        let apps = &entry.peer.apps;
        mixer::show(m, audio, entry.peer.person, is_person, apps, |sid, on| {
            acts.push(Act::Listen(sid, on))
        })
    });
    ui::menu_open(&button)
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
            Act::Full(tile) => {
                stage.full = Some(tile);
                stage.chrome = Chrome::default();
            }
            Act::Listen(sid, on) => send(home, Command::Listen(sid, on)),
            Act::Watch(screen) => watch(home, &screen),
            Act::StopWatching => {
                stage.stop_watching();
                stop_watching(home);
            }
            Act::ShowPreview => {
                stage.stop_watching();
                stop_watching(home);
            }
            Act::ToggleFps => home.layout.split.show_fps ^= true,
            Act::ToggleNerdStats => home.layout.split.nerd_stats ^= true,
            Act::Person(person, picked) => person::apply(home, index, person, picked, now),
            Act::RequestControl(sid) => send(home, Command::RequestControl(sid)),
            Act::ReleaseControl => send(home, Command::ReleaseControl),
            Act::Control(inputs) => send(home, Command::ControlInput(inputs)),
            Act::AskToType => terminal::ask(home, now),
            Act::TypeInTerminal(bytes) => terminal::send_typed(home, bytes),
            Act::PasteRefused => terminal::paste_refused(home, now),
            Act::Pip(person, change) => {
                // Full screen has no room for the camera's own tile: show the stage.
                if change == pip::Change::PopOut {
                    stage.full = None;
                }
                if change == pip::Change::PutBack && stage.focus == Some(Tile::Person(person)) {
                    stage.focus = None;
                }
                home.voice.pips.entry(person).or_default().apply(change);
            }
            Act::WatchTerminal(sharer) => terminal::watch(home, &sharer),
            Act::StopWatchingTerminal => {
                // Like a screen: a big terminal would be watched again at once.
                stage.presenting = false;
                if matches!(stage.focus, Some(Tile::Terminal(_))) {
                    stage.focus = None;
                }
                if matches!(stage.full, Some(Tile::Terminal(_))) {
                    stage.full = None;
                }
                terminal::stop_watching(home);
            }
        }
    }
}

fn send(home: &Home, command: Command) {
    if let Some(server) = call_server(home) {
        server.send(command);
    }
}

/// The full-screen buttons: finger sized, like the call bar on phones.
const FULL_TOOL: Vec2 = Vec2::splat(ui::TOUCH_TARGET);

/// How wide a panel of `count` full-screen buttons is, with its margins.
fn panel_width(count: usize) -> f32 {
    let count = count as f32;
    count * FULL_TOOL.x + (count - 1.0).max(0.0) * ui::SPACE_XS + 2.0 * ui::SPACE_XS
}

/// A raised panel at `rect` for the full-screen controls, with `add` laid out in it
/// left to right.
fn panel<R>(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui) -> R) -> R {
    let c = ui::colors(ui);
    ui.painter().rect_filled(rect, ui::RADIUS, c.side);
    ui.painter().rect_stroke(
        rect,
        ui::RADIUS,
        Stroke::new(1.0, c.border),
        StrokeKind::Inside,
    );
    let inner = rect.shrink(ui::SPACE_XS);
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::left_to_right(Align::Min)),
    );
    child.spacing_mut().item_spacing.x = ui::SPACE_XS;
    add(&mut child)
}

/// A camera, screen or terminal covering the whole window. Its name, its own buttons
/// and menus, t!("call.exit_full_screen") and the call's buttons float over it while
/// the pointer moves (a tap on phones), and fade when idle. Escape leaves.
pub fn full_view(ui: &mut Ui, home: &mut Home) {
    let Some(tile) = home.voice.stage.full.clone() else {
        return;
    };
    let ctx = ui.ctx().clone();
    let now = ui.input(|input| input.time);
    // A menu or dialog over the picture closes first on Escape.
    let covered = Popup::is_any_open(&ctx) || ctx.memory(|m| m.top_modal_layer().is_some());
    let escape = !covered && ui.input(|input| input.key_pressed(Key::Escape));
    let found = home.voice.call.as_ref().and_then(|call| {
        let index = (home.servers.iter()).position(|s| s.address == call.server)?;
        let server = &home.servers[index];
        let me = server.dir.me;
        let peer = server
            .voice
            .get(&call.channel)?
            .iter()
            .find(|peer| match &tile {
                Tile::Person(person) | Tile::Terminal(person) => peer.person == *person,
                Tile::Screen(sid) => peer.screens.iter().any(|s| s.sid == *sid),
                Tile::Preview => peer.person == me,
            });
        Some((index, peer.cloned(), me))
    });
    let Some((index, peer, me)) = found else {
        home.voice.stage.full = None;
        return;
    };
    let voice = &mut home.voice;
    let server = &home.servers[index];
    // What is shown, while it still can be.
    let shown = peer.as_ref().and_then(|peer| {
        let name = peer.name(server);
        let mine = peer.person == me;
        match &tile {
            // Camera turned off: nothing left to show big.
            Tile::Person(_) => peer.camera.is_some().then_some(name),
            Tile::Screen(_) => Some(t!("call.someones_screen", name = name)),
            Tile::Preview => previewing(voice).then(|| t!("call.your_screen_preview").to_owned()),
            Tile::Terminal(_) if mine => {
                (voice.terminals.local.is_some()).then(|| t!("call.your_terminal").to_owned())
            }
            Tile::Terminal(_) => (peer.terminal.is_some()
                && voice.terminals.watched() == peer.terminal.as_deref())
            .then(|| t!("call.someones_terminal", name = name)),
        }
    });
    // A replacement publication may still be arriving after a restart.
    let waiting = matches!(&tile, Tile::Screen(sid) if voice.retaining_watch(sid));
    let (peer, name) = match (peer, shown) {
        (Some(peer), Some(name)) => (Some(peer), name),
        _ if waiting => (None, String::new()),
        _ => {
            voice.stage.full = None;
            return;
        }
    };
    let mine = peer.as_ref().is_some_and(|peer| peer.person == me);
    let terminal = matches!(tile, Tile::Terminal(_));
    let controlling = match &tile {
        Tile::Screen(sid) => voice.control.screen(sid) == Some(true),
        Tile::Person(_) | Tile::Preview | Tile::Terminal(_) => false,
    };
    let typing = match tile {
        Tile::Terminal(_) if !mine => voice.terminals.typing,
        _ => Typing::Idle,
    };
    // Escape belongs to our own shell, one we type in and a screen we control.
    let keys_taken = controlling || (terminal && (mine || typing == Typing::On));
    let mut exit = escape && !keys_taken;

    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, Sense::click());
    let picture = match (&tile, &peer) {
        _ if peer.is_none() => None,
        (Tile::Person(_), Some(peer)) => person_picture(&ctx, voice, peer, mine),
        (Tile::Screen(sid), _) => screen_picture(&ctx, voice, sid),
        (Tile::Preview, _) => preview_picture(&ctx, voice),
        _ => None,
    };
    let mut acts = Vec::new();
    let mut picture_rect = None;
    if terminal {
        // Field by field, so the terminals stay writable.
        let terms = &mut voice.terminals;
        if let (true, Some(local)) = (mine, &mut terms.local) {
            terminal::ours(ui, rect, local, (&terms.guests, Some(server)), true, &name);
        } else {
            let typing = typing == Typing::On;
            let screen = terms
                .watching
                .as_mut()
                .and_then(|(_, screen)| screen.as_mut());
            let (typed, refused) = terminal::theirs(ui, rect, screen, (true, typing), &name);
            if !typed.is_empty() {
                acts.push(Act::TypeInTerminal(typed));
            }
            if refused {
                acts.push(Act::PasteRefused);
            }
        }
    } else if let Some(mut picture) = picture {
        // Whole, not cropped, now that it has the room.
        picture.cover = false;
        paint_picture(ui, rect, picture);
        picture_rect = Some(fitted(rect, picture.size));
        if controlling {
            let shown = fitted(rect, picture.size);
            let (inputs, stop) = voice.control.capture(ui, &response, shown);
            let accent = ui::accent(ui).base;
            ui.painter()
                .rect_stroke(shown, 0.0, Stroke::new(2.0, accent), StrokeKind::Outside);
            if !inputs.is_empty() {
                acts.push(Act::Control(inputs));
            }
            if stop {
                acts.push(Act::ReleaseControl);
            }
        }
    } else {
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            t!("call.waiting_picture"),
            FontId::proportional(ui::TEXT_BODY),
            ui::colors(ui).muted,
        );
    }
    // The sharer's camera, clear of the controls at the top and bottom.
    let camera = peer.as_ref().filter(|peer| {
        matches!(tile, Tile::Screen(_) | Tile::Preview)
            && peer.camera.is_some()
            && (mine || !voice.hidden.contains(&peer.person))
    });
    if let Some(peer) = camera {
        let pip = voice.pips.get(&peer.person).copied().unwrap_or_default();
        if !pip.popped {
            let picture = (!pip.minimized)
                .then(|| person_picture(&ctx, voice, peer, mine))
                .flatten();
            let entry = Entry {
                tile: Tile::Person(peer.person),
                peer,
                name: peer.name(server),
                me: mine,
                picture: None,
                video: true,
                watched: false,
                camera_visible: true,
                speaking: false,
                camera: None,
                popped: false,
            };
            let stream = picture_rect.unwrap_or(rect);
            let clear = rect.shrink2(vec2(0.0, 2.0 * GAP + FULL_TOOL.y + 2.0 * ui::SPACE_XS));
            let camera = OnScreen { pip, picture };
            camera_pip(ui, stream, clear, &entry, camera, server, &mut acts);
        }
    }

    // The controls: on mouse movement or a tap on the picture (phones toggle them).
    let (touch, moved, pointer) = ui.input(|input| {
        let touch = input.any_touches()
            || (input.events.iter()).any(|event| matches!(event, egui::Event::Touch { .. }));
        let moved = !touch && input.pointer.delta() != Vec2::ZERO;
        (touch, moved, input.pointer.hover_pos())
    });
    let keyboard = ui.input(|input| input.key_pressed(Key::Tab));
    // A terminal takes the taps itself, so touching it shows the controls.
    let touched = terminal && touch && ui.input(|input| input.pointer.any_pressed());
    let tapped = touch && response.clicked();
    let chrome = &mut voice.stage.chrome;
    let held = covered || chrome.over(pointer);
    let visible = chrome.update(now, moved || keyboard || touched, tapped, held);
    if visible {
        let left = chrome.left(now);
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(left));
    }
    let fade = ctx.animate_bool_with_time(
        UiId::unique("full screen controls"),
        visible,
        ui::STATE_SECONDS,
    );
    if fade == 0.0 && !controlling && response.hovered() {
        ctx.set_cursor_icon(CursorIcon::None);
    }

    let audio = voice.audio.clone();
    let split = home.layout.split;
    let stats = voice.call.as_ref().map(|call| &call.stats);
    let overlay = Overlay::new(&tile, split.show_fps, split.nerd_stats, stats);
    let watched = match &tile {
        Tile::Screen(sid) => voice.watching.as_ref().is_some_and(|w| w.sid == *sid),
        Tile::Terminal(_) => !mine,
        Tile::Person(_) | Tile::Preview => false,
    };
    let entry = peer.as_ref().map(|peer| Entry {
        tile: tile.clone(),
        peer,
        name: name.clone(),
        me: mine,
        picture,
        video: true,
        watched,
        camera_visible: peer.camera.is_some() && !voice.hidden.contains(&peer.person),
        speaking: false,
        camera: None,
        popped: false,
    });
    let state = TileState {
        relation: (peer.as_ref()).map_or(Relation::None, |p| home.friends.relation(&p.person)),
        can_ask: false,
        joined: true,
        big: true,
        spotlight: false,
        focused: false,
        remote: match &tile {
            Tile::Screen(sid) if !mine => voice.control.screen(sid),
            _ => None,
        },
        typing,
    };
    let sharer = |entry: &Entry| {
        (!entry.peer.unknown).then(|| Person {
            server,
            person: entry.peer.person,
            relation: state.relation,
            can_ask: false,
            audio: (!entry.me).then_some(&audio),
            video: (!entry.me && entry.peer.camera.is_some()).then_some(entry.camera_visible),
        })
    };
    // Right-click or a long press on the picture: the screen's menu, as on its tile.
    let mut picked = None;
    if let Some(entry) = &entry
        && matches!(tile, Tile::Screen(_) | Tile::Preview)
        && !controlling
    {
        let sharer = sharer(entry);
        let menu = ui::context_menu(&response, |m| {
            screen_menu(
                m,
                entry,
                state,
                &audio,
                sharer.as_ref(),
                &overlay,
                true,
                &mut acts,
            )
        });
        picked = picked.take().or(menu.flatten());
    }

    let mut name_tag = None;
    let mut panels = Vec::new();
    let mut overlay_ui = ui.new_child(UiBuilder::new().max_rect(rect));
    overlay_ui.multiply_opacity(fade);
    if fade > 0.0 {
        let ui = &mut overlay_ui;
        let is_person = matches!(tile, Tile::Person(_));
        let tools = entry.as_ref().map(|entry| {
            let control = !is_person && !mine && watched && (entry.picture.is_some() || terminal);
            let sound = is_person || (!terminal && !entry.peer.apps.is_empty());
            let volume = sound && !mine && !entry.peer.unknown;
            let menu = matches!(tile, Tile::Screen(_) | Tile::Preview);
            (control, volume, menu, watched)
        });
        let (control, volume, menu, stop) = tools.unwrap_or_default();
        let count = 1 + [control, volume, menu, stop]
            .iter()
            .filter(|on| **on)
            .count();
        let width = panel_width(count).min(rect.width() - 2.0 * GAP);
        let height = FULL_TOOL.y + 2.0 * ui::SPACE_XS;
        let top = Rect::from_min_size(
            pos2(rect.right() - GAP - width, rect.top() + GAP),
            vec2(width, height),
        );
        panels.push(top);
        panel(ui, top, |ui| {
            if let Some(entry) = &entry {
                if control {
                    control_button(ui, entry, state, FULL_TOOL, &mut acts);
                }
                if volume {
                    volume_button(ui, entry, &audio, FULL_TOOL, &mut acts);
                }
                if menu {
                    let sharer = sharer(entry);
                    let button =
                        ui::icon_button(ui, "…", t!("call.stream_options"), Look::Plain, FULL_TOOL);
                    let menu = ui::menu(&button, |m| {
                        screen_menu(
                            m,
                            entry,
                            state,
                            &audio,
                            sharer.as_ref(),
                            &overlay,
                            true,
                            &mut acts,
                        )
                    });
                    picked = picked.take().or(menu.flatten());
                }
                if stop {
                    let tip = t!("call.stop_watching");
                    if ui::icon_button(ui, "⏹", tip, Look::Plain, FULL_TOOL).clicked() {
                        acts.push(if terminal {
                            Act::StopWatchingTerminal
                        } else {
                            Act::StopWatching
                        });
                    }
                }
            }
            let tip = t!("call.exit_full_screen");
            exit |= ui::icon_button(ui, "⛶", tip, Look::Plain, FULL_TOOL).clicked();
        });
        // The name, in the room left of the buttons.
        let heading = if controlling {
            t!("call.controlling", name = name)
        } else {
            name.clone()
        };
        let room = top.left() - rect.left() - 3.0 * GAP;
        if !heading.is_empty() && room > FULL_TOOL.x {
            let mut job = LayoutJob::simple_singleline(
                heading,
                FontId::new(ui::TEXT_BODY, ui::bold_family()),
                ui::colors(ui).strong,
            );
            job.wrap = TextWrapping::truncate_at_width(room - 2.0 * ui::SPACE_M);
            let galley = ui.painter().layout_job(job);
            let size = vec2(galley.size().x + 2.0 * ui::SPACE_M, height);
            let tag = Rect::from_min_size(rect.min + Vec2::splat(GAP), size);
            panel(ui, tag, |ui| {
                let at = pos2(
                    tag.left() + ui::SPACE_M,
                    tag.center().y - galley.size().y / 2.0,
                );
                ui.painter().galley(at, galley, ui::colors(ui).strong);
            });
            panels.push(tag);
            name_tag = Some(tag);
        }
    }
    overlay.paint(ui, rect, name_tag);
    if fade > 0.0 {
        // The call's buttons along the bottom.
        let count = super::panel::phone_control_count(home);
        let width = panel_width(count).min(rect.width() - 2.0 * GAP);
        let height = FULL_TOOL.y + 2.0 * ui::SPACE_XS;
        let bottom = Rect::from_center_size(
            pos2(rect.center().x, rect.bottom() - GAP - height / 2.0),
            vec2(width, height),
        );
        panels.push(bottom);
        panel(&mut overlay_ui, bottom, |ui| {
            super::panel::phone_controls(ui, home)
        });
    }
    home.voice.stage.chrome.panels = panels;
    if let (Some(picked), Some(peer)) = (picked, &peer) {
        acts.push(Act::Person(peer.person, picked));
    }
    if exit {
        ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape));
        home.voice.stage.full = None;
    }
    apply(home, index, now, acts);
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
    fn multiple_devices_choose_watched_then_local_then_stable_remote() {
        let mut peer = Peer {
            person: [1; 32],
            connecting: false,
            unknown: false,
            status: VoiceStatus::default(),
            apps: vec![],
            camera: None,
            terminal: None,
            screens: vec![
                Screen {
                    sid: "a".into(),
                    name: "A".into(),
                    viewers: 2,
                    local: false,
                    control: None,
                },
                Screen {
                    sid: "b".into(),
                    name: "B".into(),
                    viewers: 0,
                    local: true,
                    control: None,
                },
            ],
        };
        assert_eq!(selected_screen(&peer, None).unwrap().sid, "b");
        assert_eq!(selected_screen(&peer, Some("a")).unwrap().sid, "a");
        peer.screens.pop();
        assert_eq!(selected_screen(&peer, Some("b")).unwrap().sid, "a");
        peer.screens.clear();
        assert!(selected_screen(&peer, None).is_none());
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
    fn republished_screen_keeps_focus_and_fullscreen() {
        let mut stage = Stage {
            focus: Some(Tile::Screen("old".into())),
            full: Some(Tile::Screen("old".into())),
            ..Stage::default()
        };
        stage.replace_video("other", "unrelated");
        assert_eq!(stage.focus, Some(Tile::Screen("old".into())));
        stage.replace_video("old", "new");
        assert_eq!(stage.focus, Some(Tile::Screen("new".into())));
        assert_eq!(stage.full, stage.focus);
    }

    #[test]
    fn full_screen_controls_show_while_used_and_hide_when_idle() {
        let mut chrome = Chrome::default();
        // Shown on entering, so the way out is seen, then gone after a quiet spell.
        assert!(chrome.update(10.0, false, false, false));
        assert!(chrome.update(10.0 + IDLE - 0.1, false, false, false));
        assert!(!chrome.update(10.0 + IDLE, false, false, false));
        // Moving the pointer brings them back for another spell.
        assert!(chrome.update(20.0, true, false, false));
        assert!(chrome.update(20.0 + IDLE - 0.1, false, false, false));
        assert!(!chrome.update(20.0 + IDLE, false, false, false));
        // An open menu, or the pointer on them, keeps them however long it takes.
        assert!(chrome.update(30.0, false, false, true));
        assert!(chrome.update(60.0, false, false, true));
        assert!(chrome.update(60.0 + IDLE - 0.1, false, false, false));
        assert!(!chrome.update(60.0 + IDLE, false, false, false));
        chrome.panels = vec![Rect::from_min_size(pos2(0.0, 0.0), vec2(50.0, 50.0))];
        assert!(chrome.over(Some(pos2(10.0, 10.0))));
        assert!(!chrome.over(Some(pos2(60.0, 10.0))) && !chrome.over(None));
    }

    #[test]
    fn a_tap_toggles_the_full_screen_controls() {
        let mut chrome = Chrome::default();
        assert!(chrome.update(0.0, false, false, false));
        // Shown: a tap hides them at once, the next one shows them again.
        assert!(!chrome.update(0.5, false, true, false));
        assert!(!chrome.update(1.0, false, false, false));
        assert!(chrome.update(1.5, false, true, false));
        assert!((chrome.left(1.5) - IDLE).abs() < 1e-9);
        // And they still hide by themselves.
        assert!(!chrome.update(1.5 + IDLE, false, false, false));
    }

    #[test]
    fn stopping_the_full_screen_stream_leaves_full_screen() {
        let mut stage = Stage {
            full: Some(Tile::Screen("s".into())),
            ..Stage::default()
        };
        stage.stop_watching();
        assert_eq!(stage.full, None);
        // A camera in full screen stays.
        stage.full = Some(Tile::Person([0; 32]));
        stage.stop_watching();
        assert!(stage.full.is_some());
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
