//! The picker over the composer: Stickers and Emoji, a search, a row of tabs (recent,
//! favourites, one per pack) and a grid. On a desktop it is a popover and a click sends;
//! on a phone it is docked under the composer in place of the keyboard, and a tapped
//! sticker shows large above the composer first, so a mis-tap sends nothing.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use eframe::egui::{
    self, Align, Align2, FontId, Frame, Layout, Margin, PointerButton, Rect, Response, ScrollArea,
    Sense, Stroke, TextBuffer as _, TextEdit, Ui, UiBuilder, Vec2,
    text::{CCursor, CCursorRange},
};
use rusttalk_core::{
    auth::{decode, encode},
    client::save_json,
};

use super::{PICKER, SHORTCODES, matching, shortcode_char, sprite, text};
use crate::{
    server::{CustomEmoji, Directory, Id, Picture},
    ui,
};

pub const RECENT_FILE: &str = "emoji-recent.json";
pub const FAVOURITES_FILE: &str = "emoji-favourites.json";
pub const STICKER_RECENT_FILE: &str = "sticker-recent.json";
const RECENT_LIMIT: usize = 32;
const FAVOURITES_LIMIT: usize = 64;
/// Four rows of the sticker grid.
const STICKER_RECENT_LIMIT: usize = 16;
const COLUMNS: usize = 10;
const CELL: f32 = 36.0;
const STICKER_COLUMNS: usize = 4;
const STICKER_CELL: f32 = 92.0;
/// The section headers in the emoji grid.
const HEADER: f32 = 28.0;
const BODY: f32 = 300.0;
const PADDING: f32 = 12.0;
/// The panel's side padding and room for the grid's scroll bar.
const INSET: f32 = 2.0 * PADDING + 4.0;
/// A sticker tapped on a phone, shown above the composer.
const PREVIEW: f32 = 140.0;

/// Phones and touch screens: the picker docks under the composer in place of the
/// keyboard, and a sticker is previewed before it is sent.
pub fn docked(ctx: &egui::Context) -> bool {
    ui::TOUCH || ui::is_phone(ctx)
}

/// The picker's sizes this frame.
struct Metrics {
    docked: bool,
    width: f32,
    /// The grid's height.
    body: f32,
    cell: f32,
    columns: usize,
    sticker_cell: f32,
}

impl Metrics {
    /// A popover as wide as four stickers, or docked across `docked` points.
    fn new(ctx: &egui::Context, docked: Option<f32>) -> Self {
        let width =
            docked.unwrap_or_else(|| ui::fit(ctx, STICKER_COLUMNS as f32 * STICKER_CELL + INSET));
        let inner = (width - INSET).max(CELL);
        // Fingers need bigger emoji; the panel takes about a keyboard's height.
        let (cell, body) = match docked {
            Some(_) => (
                ui::TOUCH_TARGET,
                (ctx.content_rect().height() * 0.28).clamp(160.0, 300.0),
            ),
            None => (CELL, BODY),
        };
        Self {
            docked: docked.is_some(),
            width,
            body,
            cell,
            columns: ((inner / cell).floor() as usize).clamp(1, COLUMNS),
            sticker_cell: (inner / STICKER_COLUMNS as f32).floor(),
        }
    }

    /// The tab buttons.
    fn tab(&self) -> f32 {
        if self.docked {
            ui::TOUCH_TARGET
        } else {
            ui::ICON_BUTTON
        }
    }
}

/// The picker's tabs, each opened by its own composer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Stickers,
    Emoji,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Stickers => t!("picker.stickers"),
            Tab::Emoji => t!("picker.emoji"),
        }
    }
}

/// A group of emoji or stickers, and its tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Section {
    Favourites,
    Recent,
    /// One pack of server emoji or stickers, by its place among the packs shown.
    Pack(usize),
    Unicode(u8),
    /// Search results, without a header.
    Found,
}

const UNICODE: [u8; 9] = [0, 1, 3, 4, 5, 6, 7, 8, 9];

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Favourites => t!("picker.favourites"),
            Section::Recent => t!("picker.recent"),
            Section::Pack(_) => "",
            Section::Unicode(0) => t!("picker.smileys"),
            Section::Unicode(1) => t!("picker.people"),
            Section::Unicode(3) => t!("picker.animals"),
            Section::Unicode(4) => t!("picker.food"),
            Section::Unicode(5) => t!("picker.travel"),
            Section::Unicode(6) => t!("picker.activity"),
            Section::Unicode(7) => t!("picker.objects"),
            Section::Unicode(8) => t!("picker.symbols"),
            Section::Unicode(_) => t!("picker.flags"),
            Section::Found => "",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Section::Favourites => "★",
            Section::Recent => "🕘",
            Section::Pack(_) => "🏠",
            Section::Unicode(0) => "☺",
            Section::Unicode(1) => "👋",
            Section::Unicode(3) => "🐻",
            Section::Unicode(4) => "🍔",
            Section::Unicode(5) => "✈",
            Section::Unicode(6) => "⚽",
            Section::Unicode(7) => "💡",
            Section::Unicode(8) => "🔣",
            Section::Unicode(_) => "🏁",
            Section::Found => "🔍",
        }
    }

    fn header(self) -> f32 {
        if self == Section::Found { 0.0 } else { HEADER }
    }
}

fn tones() -> [(&'static str, &'static str); 6] {
    [
        ("👋", t!("picker.tone_default")),
        ("👋🏻", t!("picker.tone_light")),
        ("👋🏼", t!("picker.tone_medium_light")),
        ("👋🏽", t!("picker.tone_medium")),
        ("👋🏾", t!("picker.tone_medium_dark")),
        ("👋🏿", t!("picker.tone_dark")),
    ]
}

#[derive(Clone, Copy)]
enum Item<'a> {
    /// Sequence and name.
    Unicode(&'a str, &'a str),
    /// This server's emoji, typed into the text by name.
    Custom(&'a CustomEmoji),
    /// Another server's: sent as a small file along with the message.
    Foreign(&'a CustomEmoji),
}

struct Group<'a> {
    section: Section,
    /// For pack sections; the others are named by their section.
    title: Option<String>,
    items: Vec<Item<'a>>,
}

impl Group<'_> {
    fn title(&self) -> &str {
        self.title
            .as_deref()
            .unwrap_or_else(|| self.section.title())
    }
}

/// The emoji and stickers of one pack, for what is shown now.
struct PackItems<'a> {
    title: String,
    foreign: bool,
    items: Vec<&'a CustomEmoji>,
}

/// One sticker tab: its stickers, each with whether it is another server's.
struct Stickers<'a> {
    section: Section,
    title: String,
    items: Vec<(&'a CustomEmoji, bool)>,
}

/// A tab in the row across the top, drawn with a pack's first picture if it has one.
struct TabButton<'a> {
    section: Section,
    title: &'a str,
    picture: Option<&'a Picture>,
}

/// The server emoji the picker offers: this server's, which go into the text by ID, and
/// other servers', which go along as files. In a DM nothing is this server's.
pub struct Scope<'a> {
    pub local: Option<&'a Directory>,
    pub others: Vec<(&'a str, &'a Directory)>,
}

impl<'a> Scope<'a> {
    /// The packs with emoji (or `sticker`s) whose names contain `query`, this server's
    /// first, then each other server's.
    fn packs(&self, sticker: bool, query: &str) -> Vec<PackItems<'a>> {
        let sources = (self.local.map(|dir| (None, dir)).into_iter())
            .chain(self.others.iter().map(|(name, dir)| (Some(*name), *dir)));
        let mut out = Vec::new();
        for (server, dir) in sources {
            // A server that has no packs yet keeps everything in one.
            let none = [crate::server::EmojiPack {
                id: [0; 32],
                name: None,
            }];
            let packs = if dir.packs.is_empty() {
                &none[..]
            } else {
                &dir.packs[..]
            };
            for pack in packs {
                let items: Vec<_> = (dir.emoji.iter())
                    .filter(|e| e.pack == pack.id && e.sticker == sticker && e.name.contains(query))
                    .collect();
                if items.is_empty() {
                    continue;
                }
                let title = match server {
                    None => pack.title(),
                    Some(name) if packs.len() <= 1 => name.to_owned(),
                    Some(name) => format!("{name} · {}", pack.title()),
                };
                out.push(PackItems {
                    title,
                    foreign: server.is_some(),
                    items,
                });
            }
        }
        out
    }

    /// Every emoji and sticker offered, to look a hovered one up by ID.
    fn all(&self) -> impl Iterator<Item = &'a CustomEmoji> + '_ {
        (self.local.into_iter())
            .chain(self.others.iter().map(|(_, dir)| *dir))
            .flat_map(|dir| dir.emoji.iter())
    }

    /// An emoji or sticker by ID, and whether it is another server's.
    fn find(&self, id: &Id) -> Option<(&'a CustomEmoji, bool)> {
        let local = (self.local.into_iter()).flat_map(|dir| dir.emoji.iter().map(|e| (e, false)));
        let others = (self.others.iter()).flat_map(|(_, dir)| dir.emoji.iter().map(|e| (e, true)));
        local.chain(others).find(|(emoji, _)| emoji.id == *id)
    }
}

/// What picking a server emoji or sticker means: text or a sticker of this server, or a
/// picture of another server to send as a file.
fn pick_custom(emoji: &CustomEmoji, foreign: bool) -> Picked {
    match (foreign, emoji.sticker) {
        (true, _) => Picked::Foreign(emoji.clone()),
        (false, true) => Picked::Sticker(emoji.id, emoji.name.clone()),
        (false, false) => Picked::Text(format!(":{}:", emoji.name)),
    }
}

/// What the pointer was last over, for the bar at the bottom.
#[derive(Clone)]
enum Hover {
    Unicode(String),
    Custom(Id),
}

/// What was picked: text to insert, or a sticker to send (ID and name).
pub enum Picked {
    Text(String),
    Sticker(Id, String),
    /// An emoji or sticker of another server.
    Foreign(CustomEmoji),
}

pub struct Picker {
    data: PathBuf,
    recent: Vec<String>,
    favourites: Vec<String>,
    /// Stickers sent from the picker, newest first, by encoded ID.
    sticker_recent: Vec<String>,
    open: bool,
    tab: Tab,
    query: String,
    tone: usize,
    /// The section a tab asked for, scrolled to on the next frame.
    jump: Option<Section>,
    /// The section at the top of the emoji grid, lit in the tabs.
    current: Option<Section>,
    /// The sticker tab shown.
    sticker_tab: Option<Section>,
    /// The tab lit last frame, to bring a newly lit one into view.
    lit: Option<Section>,
    hover: Option<Hover>,
    focus_search: bool,
    pressed_cursor: Option<CCursorRange>,
    insert_at: Option<CCursorRange>,
    /// Opened to pick a reaction: emoji only.
    reaction: bool,
    /// Docked: the sticker tapped, shown large above the composer until sent or put away.
    preview: Option<Id>,
}

fn load(path: &Path) -> Vec<String> {
    let stored = crate::load_json::<Vec<String>>(path).unwrap_or_else(|error| {
        tracing::warn!(%error, "reading saved emoji failed");
        Vec::new()
    });
    let mut list = Vec::new();
    for sequence in (stored.iter().rev()).filter(|sequence| sprite(sequence).is_some()) {
        promote(&mut list, sequence, usize::MAX);
    }
    list
}

impl Picker {
    pub fn new(data: &Path) -> Self {
        let sticker_recent = crate::load_json::<Vec<String>>(&data.join(STICKER_RECENT_FILE))
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "reading recent stickers failed");
                Vec::new()
            });
        Self {
            data: data.to_owned(),
            recent: load(&data.join(RECENT_FILE)),
            favourites: load(&data.join(FAVOURITES_FILE)),
            sticker_recent,
            open: false,
            tab: Tab::Emoji,
            query: String::new(),
            tone: 0,
            jump: None,
            current: None,
            sticker_tab: None,
            lit: None,
            hover: None,
            focus_search: false,
            pressed_cursor: None,
            insert_at: None,
            reaction: false,
            preview: None,
        }
    }

    pub fn is_open(&self, tab: Tab) -> bool {
        self.open && self.tab == tab
    }

    pub fn close(&mut self) {
        self.open = false;
        self.preview = None;
    }

    pub fn remember(&mut self, sequence: &str) {
        // Server emoji belong to one server, so only Unicode emoji are recent.
        if sprite(sequence).is_none() || self.recent.first().is_some_and(|first| first == sequence)
        {
            return;
        }
        promote(&mut self.recent, sequence, RECENT_LIMIT);
        self.store(RECENT_FILE, &self.recent);
    }

    fn toggle_favourite(&mut self, sequence: &str) {
        if self.favourites.iter().any(|item| item == sequence) {
            self.favourites.retain(|item| item != sequence);
        } else if self.favourites.len() < FAVOURITES_LIMIT {
            self.favourites.push(sequence.to_owned());
        }
        self.store(FAVOURITES_FILE, &self.favourites);
    }

    fn store(&self, file: &str, list: &[String]) {
        if let Err(error) = save_json(&self.data, &self.data.join(file), &list) {
            tracing::warn!(%error, "saving emoji failed");
        }
    }

    /// Pick a server emoji or sticker; a sticker becomes the newest recent one.
    fn pick(&mut self, emoji: &CustomEmoji, foreign: bool) -> Picked {
        if emoji.sticker {
            promote(
                &mut self.sticker_recent,
                &encode(&emoji.id),
                STICKER_RECENT_LIMIT,
            );
            self.store(STICKER_RECENT_FILE, &self.sticker_recent);
        }
        pick_custom(emoji, foreign)
    }

    /// A composer button that opens the panel on `tab`, or closes it when it is open there.
    /// Call after `show`, which closes the panel on a click outside it.
    pub fn button(&mut self, response: &Response, tab: Tab, editor: egui::Id, was_open: bool) {
        let cursor = || {
            TextEdit::load_state(&response.ctx, editor).and_then(|state| state.cursor.char_range())
        };
        // Opening the panel focuses its search box, which collapses the editor's selection.
        if response.ctx.input(|input| input.pointer.primary_pressed())
            && response.is_pointer_button_down_on()
        {
            self.pressed_cursor = cursor();
        }
        if !response.clicked() {
            return;
        }
        let pressed = self.pressed_cursor.take();
        if was_open && self.tab == tab {
            self.close();
            return;
        }
        self.insert_at = if response.clicked_by(PointerButton::Primary) {
            pressed.or_else(cursor)
        } else {
            cursor()
        };
        // Docked, the panel takes the keyboard's place.
        if docked(&response.ctx) {
            response
                .ctx
                .memory_mut(|memory| memory.surrender_focus(editor));
        }
        self.open = true;
        self.reaction = false;
        self.tab = tab;
        self.preview = None;
        self.query.clear();
        self.hover = None;
        self.focus_search = true;
    }

    /// Open for a reaction, and not since reopened from the composer.
    pub fn is_reacting(&self) -> bool {
        self.open && self.reaction
    }

    /// Open the emoji tab without a composer button, to pick a reaction.
    pub fn open_for_reaction(&mut self, ctx: &egui::Context, editor: egui::Id) {
        // Docked, the panel takes the keyboard's place.
        if docked(ctx) {
            ctx.memory_mut(|memory| memory.surrender_focus(editor));
        }
        self.open = true;
        self.reaction = true;
        self.tab = Tab::Emoji;
        self.query.clear();
        self.hover = None;
        self.focus_search = true;
        self.insert_at = None;
        self.preview = None;
    }

    /// The panel while open: above `anchor` on a desktop, or here (under the composer)
    /// when docked. A click outside it closes it, except in `keep`: the message box on
    /// a desktop, so typing goes on; the composer when docked, whose box swaps the
    /// panel for the keyboard instead (see `keyboard_took_over`). Returns whether it was
    /// open before this frame, for `button`, and what was picked.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        (anchor, keep): (Rect, Rect),
        scope: &Scope,
    ) -> (bool, Option<Picked>) {
        let was_open = self.open;
        let picked = if !docked(ui.ctx()) {
            let metrics = Metrics::new(ui.ctx(), None);
            let mut open = self.open;
            let id = egui::Id::unique("picker");
            let picked = ui::panel_popup(ui, id, (anchor, keep), &mut open, |ui| {
                ui.set_width(metrics.width);
                self.panel(ui, scope, &metrics)
            })
            .flatten();
            self.open = open && picked.is_none();
            picked
        } else if self.open {
            ui.add_space(ui::SPACE_S);
            // Less the frame's border on each side.
            let metrics = Metrics::new(ui.ctx(), Some(ui.available_width() - 2.0));
            let c = ui::colors(ui);
            let panel = Frame::new()
                .fill(c.raised)
                .stroke(Stroke::new(1.0, c.border))
                .corner_radius(ui::RADIUS_WINDOW)
                .show(ui, |ui| self.panel(ui, scope, &metrics));
            // A tap on the chat or the header puts it away; a drag (scrolling) does not.
            let outside = ui.input(|input| {
                input.pointer.any_click()
                    && (input.pointer.interact_pos())
                        .is_some_and(|at| !panel.response.rect.contains(at) && !keep.contains(at))
            });
            let picked = panel.inner;
            // It stays open for the next one, as a keyboard would; a reaction is one.
            if outside || picked.is_some() && self.reaction {
                self.close();
            }
            picked
        } else {
            None
        };
        if !self.open {
            self.preview = None;
        }
        if let Some(Picked::Text(sequence)) = &picked {
            self.remember(sequence);
        }
        (was_open, picked)
    }

    /// Docked, the panel and the keyboard are never up together: the message box
    /// `editor` taking focus (a tap on it, or Edit) closes the panel, and the panel
    /// opening (`button`, `open_for_reaction`) takes the focus away, which hides the
    /// keyboard (egui-winit stops the IME; on Android `android::frame_end` hides it).
    pub fn keyboard_took_over(&mut self, ctx: &egui::Context, editor: egui::Id) {
        if self.open && docked(ctx) && ctx.memory(|memory| memory.has_focus(editor)) {
            self.close();
        }
    }

    /// Docked: whether a sticker waits above the composer, which Send then sends.
    pub fn has_preview(&self) -> bool {
        self.open && self.preview.is_some()
    }

    /// The sticker waiting above the composer, to send it.
    pub fn send_preview(&mut self, scope: &Scope) -> Option<Picked> {
        let id = self.preview.take().filter(|_| self.open)?;
        let (emoji, foreign) = scope.find(&id)?;
        Some(self.pick(emoji, foreign))
    }

    /// Docked: the tapped sticker large, above the composer, with a ×. Tapping it sends
    /// it: returns it then.
    pub fn preview(&mut self, ui: &mut Ui, scope: &Scope) -> Option<Picked> {
        let id = self.preview.filter(|_| self.open)?;
        let Some((emoji, foreign)) = scope.find(&id) else {
            self.preview = None;
            return None;
        };
        let (row, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), PREVIEW + 2.0 * ui::SPACE_S),
            Sense::hover(),
        );
        let spot = Rect::from_center_size(row.center(), Vec2::splat(PREVIEW));
        let card = spot.expand(ui::SPACE_S);
        let c = ui::colors(ui);
        ui.painter().rect(
            card,
            ui::RADIUS_WINDOW,
            c.raised,
            Stroke::new(1.0, c.border),
            egui::StrokeKind::Inside,
        );
        let label = t!("picker.send_sticker", name = format!(":{}:", emoji.name));
        let response = ui.interact(spot, egui::Id::unique("sticker preview"), Sense::click());
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
        match &emoji.picture {
            Some(picture) => crate::home::paint_live(ui, spot, picture, 0.0),
            None => {
                ui.painter().text(
                    spot.center(),
                    Align2::CENTER_CENTER,
                    format!(":{}:", emoji.name),
                    FontId::proportional(ui::TEXT_BODY),
                    c.muted,
                );
            }
        }
        let close = Rect::from_min_size(
            card.right_top() + Vec2::new(-22.0 - ui::SPACE_XS, ui::SPACE_XS),
            Vec2::splat(22.0),
        );
        // A child, so the × leaves the composer's layout alone.
        let mut corner = ui.new_child(UiBuilder::new().max_rect(close));
        if ui::small_close(&mut corner, t!("picker.close_preview")).clicked() {
            self.preview = None;
            return None;
        }
        if !response.clicked() {
            return None;
        }
        self.preview = None;
        Some(self.pick(emoji, foreign))
    }

    fn panel(&mut self, ui: &mut Ui, scope: &Scope, m: &Metrics) -> Option<Picked> {
        ui.spacing_mut().item_spacing = Vec2::splat(ui::SPACE_S);
        Frame::new()
            .inner_margin(Margin {
                bottom: ui::SPACE_S as i8,
                ..Margin::same(PADDING as i8)
            })
            .show(ui, |ui| {
                ui.set_width(m.width - 2.0 * PADDING);
                ui.horizontal(|ui| {
                    // A reaction is an emoji, so picking one shows no sticker tab.
                    let tabs: &[Tab] = if self.reaction {
                        &[Tab::Emoji]
                    } else {
                        &[Tab::Stickers, Tab::Emoji]
                    };
                    for &tab in tabs {
                        if ui::choice_button(ui, self.tab == tab, tab.label(), Vec2::ZERO).clicked()
                            && self.tab != tab
                        {
                            self.tab = tab;
                            self.preview = None;
                            self.query.clear();
                            self.hover = None;
                            self.focus_search = true;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    let tone = self.tab == Tab::Emoji;
                    let width = ui.available_width() - if tone { CELL + ui::SPACE_S } else { 0.0 };
                    let hint = match self.tab {
                        Tab::Stickers => t!("picker.search_stickers"),
                        Tab::Emoji => t!("picker.search"),
                    };
                    let search = ui.add(
                        ui::field(&mut self.query)
                            .hint_text(hint)
                            .desired_width(width),
                    );
                    // Docked, a focused search box would bring the keyboard back up.
                    if std::mem::take(&mut self.focus_search) && !m.docked {
                        search.request_focus();
                    }
                    if search.changed() {
                        self.hover = None;
                    }
                    if tone {
                        self.tone_button(ui);
                    }
                });
            });
        line(ui, ui.cursor().top());
        let picked = match self.tab {
            Tab::Emoji => self.emoji(ui, scope, m),
            Tab::Stickers => self.stickers(ui, scope, m),
        };
        // Nothing is hovered on a touch screen.
        if m.docked {
            ui.add_space(ui::SPACE_XS);
        } else {
            line(ui, ui.cursor().top());
            self.bar(ui, scope, m);
        }
        picked
    }

    fn tone_button(&mut self, ui: &mut Ui) {
        let tones = tones();
        ui.spacing_mut().button_padding = Vec2::ZERO;
        let (sequence, name) = tones[self.tone];
        let label = format!("{}: {name}", t!("picker.skin_tone"));
        let response = ui::choice_button(ui, false, text(sequence).size(20.0), Vec2::splat(CELL))
            .on_hover_text(&label);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
        // A click moves on to the next tone, as the button shows.
        if response.clicked() {
            self.tone = (self.tone + 1) % tones.len();
        }
    }

    /// The row of tabs across the top; returns the one clicked.
    fn tabs(
        &mut self,
        ui: &mut Ui,
        m: &Metrics,
        tabs: &[TabButton],
        lit: Section,
    ) -> Option<Section> {
        let size = m.tab();
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(m.width, size + 2.0 * ui::SPACE_XS),
            Sense::hover(),
        );
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect.shrink2(Vec2::new(PADDING, ui::SPACE_XS)))
                .layout(Layout::left_to_right(Align::Center)),
        );
        let newly_lit = self.lit != Some(lit);
        self.lit = Some(lit);
        let mut clicked = None;
        ScrollArea::horizontal()
            .id_salt(("picker-tabs", self.tab == Tab::Emoji))
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(&mut child, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for tab in tabs {
                    let on = tab.section == lit;
                    let look = if on { ui::Look::Plain } else { ui::Look::Ghost };
                    let icon = if tab.picture.is_some() {
                        ""
                    } else {
                        tab.section.icon()
                    };
                    let response = ui::icon_button(ui, icon, tab.title, look, Vec2::splat(size));
                    if let Some(picture) = tab.picture {
                        let inner = Rect::from_center_size(
                            response.rect.center(),
                            Vec2::splat(size - 10.0),
                        );
                        crate::home::paint_live(ui, inner, picture, 4.0);
                    }
                    if on && newly_lit {
                        response.scroll_to_me(None);
                    }
                    if response.clicked() {
                        clicked = Some(tab.section);
                    }
                }
            });
        clicked
    }

    fn emoji(&mut self, ui: &mut Ui, scope: &Scope, m: &Metrics) -> Option<Picked> {
        let saved = Saved {
            favourites: self.favourites.clone(),
            recent: self.recent.clone(),
            query: self.query.trim().trim_matches(':').to_lowercase(),
            tone: self.tone,
        };
        let groups = saved.groups(scope);
        let searching = !self.query.trim().is_empty();
        if !searching && let Some(first) = groups.first() {
            let tabs: Vec<_> = (groups.iter())
                .map(|group| TabButton {
                    section: group.section,
                    title: group.title(),
                    // A pack's first emoji stands for it.
                    picture: match (group.section, group.items.first()) {
                        (Section::Pack(_), Some(Item::Custom(e) | Item::Foreign(e))) => {
                            e.picture.as_ref()
                        }
                        _ => None,
                    },
                })
                .collect();
            let lit = self.current.unwrap_or(first.section);
            if let Some(section) = self.tabs(ui, m, &tabs, lit) {
                self.jump = Some(section);
            }
        }
        let cell = m.cell;
        let columns = m.columns;
        body(ui, m, |ui| {
            if groups.iter().all(|group| group.items.is_empty()) {
                ui.add_space(PADDING);
                ui.label(t!("picker.none_found"));
                return None;
            }
            let mut tops = Vec::with_capacity(groups.len());
            let mut total = 0.0;
            for group in &groups {
                tops.push(total);
                total += group.section.header() + group.items.len().div_ceil(columns) as f32 * cell;
            }
            let mut area = ScrollArea::vertical()
                .id_salt(("picker-grid", searching))
                .auto_shrink(false);
            if let Some(at) = (self.jump.take())
                .and_then(|section| groups.iter().position(|group| group.section == section))
            {
                area = area.vertical_scroll_offset(tops[at]);
            }
            let mut picked = None;
            let mut favourite = None;
            area.show_viewport(ui, |ui, viewport| {
                let origin = ui.max_rect().min;
                ui.set_width(columns as f32 * cell);
                ui.set_height(total);
                for (group, top) in groups.iter().zip(&tops) {
                    let header = group.section.header();
                    let rows = group.items.len().div_ceil(columns);
                    if top + header + rows as f32 * cell < viewport.min.y || *top > viewport.max.y {
                        continue;
                    }
                    if header > 0.0 {
                        let rect = Rect::from_min_size(
                            origin + Vec2::new(4.0, *top),
                            Vec2::new(columns as f32 * cell, header),
                        );
                        ui.painter().text(
                            rect.left_center(),
                            Align2::LEFT_CENTER,
                            format!("{}  {}", group.section.icon(), group.title()),
                            FontId::new(ui::TEXT_SMALL, ui::bold_family()),
                            ui::colors(ui).muted,
                        );
                    }
                    // Only the rows in view are laid out, so only those play.
                    let first = ((viewport.min.y - top - header) / cell).floor().max(0.0) as usize;
                    let last = (((viewport.max.y - top - header) / cell).ceil().max(0.0) as usize)
                        .min(rows);
                    for row in first..last {
                        let rect = Rect::from_min_size(
                            origin + Vec2::new(0.0, top + header + row as f32 * cell),
                            Vec2::new(columns as f32 * cell, cell),
                        );
                        let builder = UiBuilder::new()
                            .max_rect(rect)
                            .id_salt((group.section, row))
                            .layout(Layout::left_to_right(Align::Center));
                        ui.scope_builder(builder, |ui| {
                            for item in group.items.iter().skip(row * columns).take(columns) {
                                let (response, hover, choice) = match *item {
                                    Item::Unicode(sequence, label) => (
                                        tile(ui, sequence, label, cell),
                                        Hover::Unicode(sequence.to_owned()),
                                        Picked::Text(sequence.to_owned()),
                                    ),
                                    Item::Custom(emoji) | Item::Foreign(emoji) => (
                                        picture_tile(ui, emoji, cell),
                                        Hover::Custom(emoji.id),
                                        pick_custom(emoji, matches!(*item, Item::Foreign(_))),
                                    ),
                                };
                                if response.hovered() {
                                    self.hover = Some(hover);
                                }
                                if let Item::Unicode(sequence, _) = *item {
                                    let faved = self.favourites.iter().any(|f| f == sequence);
                                    ui::context_menu(&response, |m| {
                                        let label = if faved {
                                            t!("picker.unfavourite")
                                        } else {
                                            t!("picker.favourite")
                                        };
                                        if m.item("★", label).clicked() {
                                            favourite = Some(sequence.to_owned());
                                        }
                                    });
                                }
                                if response.clicked() {
                                    picked = Some(choice);
                                }
                            }
                        });
                    }
                }
                self.current = (groups.iter().zip(&tops))
                    .rev()
                    .find(|(_, top)| **top <= viewport.min.y + 1.0)
                    .map(|(group, _)| group.section);
            });
            if let Some(sequence) = favourite {
                self.toggle_favourite(&sequence);
            }
            picked
        })
    }

    /// The sticker tabs: recent ones, then one per pack; or what a search found.
    fn sticker_groups<'a>(&self, scope: &Scope<'a>, query: &str) -> Vec<Stickers<'a>> {
        let packs = scope.packs(true, query);
        let mark = |pack: &PackItems<'a>| -> Vec<(&'a CustomEmoji, bool)> {
            pack.items.iter().map(|e| (*e, pack.foreign)).collect()
        };
        if !query.is_empty() {
            let items: Vec<_> = packs.iter().flat_map(mark).collect();
            return (!items.is_empty())
                .then(|| Stickers {
                    section: Section::Found,
                    title: String::new(),
                    items,
                })
                .into_iter()
                .collect();
        }
        let recent: Vec<_> = (self.sticker_recent.iter())
            .filter_map(|id| scope.find(&decode(id).ok()?))
            .filter(|(emoji, _)| emoji.sticker)
            .collect();
        let recent = (!recent.is_empty()).then(|| Stickers {
            section: Section::Recent,
            title: Section::Recent.title().to_owned(),
            items: recent,
        });
        recent
            .into_iter()
            .chain(packs.iter().enumerate().map(|(at, pack)| Stickers {
                section: Section::Pack(at),
                title: pack.title.clone(),
                items: mark(pack),
            }))
            .collect()
    }

    fn stickers(&mut self, ui: &mut Ui, scope: &Scope, m: &Metrics) -> Option<Picked> {
        let query = self.query.trim().trim_matches(':').to_lowercase();
        let groups = self.sticker_groups(scope, &query);
        let shown = (self.sticker_tab)
            .filter(|section| groups.iter().any(|group| group.section == *section))
            .or(groups.first().map(|group| group.section));
        let Some(mut shown) = shown else {
            body(ui, m, |ui| {
                ui.add_space(PADDING);
                ui.label(if scope.packs(true, "").is_empty() {
                    t!("picker.no_stickers")
                } else {
                    t!("settings.nothing_found")
                });
            });
            return None;
        };
        if query.is_empty() {
            let tabs: Vec<_> = (groups.iter())
                .map(|group| TabButton {
                    section: group.section,
                    title: &group.title,
                    picture: match group.section {
                        Section::Pack(_) => {
                            group.items.first().and_then(|(e, _)| e.picture.as_ref())
                        }
                        _ => None,
                    },
                })
                .collect();
            if let Some(section) = self.tabs(ui, m, &tabs, shown) {
                self.sticker_tab = Some(section);
                shown = section;
            }
        }
        let items = &(groups.iter().find(|group| group.section == shown))?.items;
        let cell = m.sticker_cell;
        let rows = items.len().div_ceil(STICKER_COLUMNS);
        let mut tapped = None;
        body(ui, m, |ui| {
            ScrollArea::vertical()
                .id_salt(("picker-stickers", shown))
                .auto_shrink(false)
                .show_viewport(ui, |ui, viewport| {
                    let origin = ui.max_rect().min;
                    ui.set_width(STICKER_COLUMNS as f32 * cell);
                    ui.set_height(rows as f32 * cell);
                    // Only the rows in view are laid out, so only those play.
                    let first = (viewport.min.y / cell).floor().max(0.0) as usize;
                    let last = ((viewport.max.y / cell).ceil().max(0.0) as usize).min(rows);
                    for row in first..last {
                        let rect = Rect::from_min_size(
                            origin + Vec2::new(0.0, row as f32 * cell),
                            Vec2::new(STICKER_COLUMNS as f32 * cell, cell),
                        );
                        let builder = UiBuilder::new()
                            .max_rect(rect)
                            .id_salt(("sticker-row", row))
                            .layout(Layout::left_to_right(Align::Center));
                        ui.scope_builder(builder, |ui| {
                            let cells = items.iter().skip(row * STICKER_COLUMNS);
                            for &(item, foreign) in cells.take(STICKER_COLUMNS) {
                                let response = picture_tile(ui, item, cell);
                                if response.hovered() {
                                    self.hover = Some(Hover::Custom(item.id));
                                }
                                if response.clicked() {
                                    tapped = Some((item, foreign));
                                }
                            }
                        });
                    }
                });
        });
        let (item, foreign) = tapped?;
        if m.docked {
            self.preview = Some(item.id);
            return None;
        }
        Some(self.pick(item, foreign))
    }

    /// The hovered emoji large with its `:shortcode:`, or a tip.
    fn bar(&self, ui: &mut Ui, scope: &Scope, m: &Metrics) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(m.width, 48.0), Sense::hover());
        let mut ui = ui.new_child(
            UiBuilder::new()
                .max_rect(rect.shrink2(Vec2::new(PADDING, 6.0)))
                .layout(Layout::left_to_right(Align::Center)),
        );
        let custom_emoji = |id: &Id| scope.all().find(|emoji| emoji.id == *id);
        match &self.hover {
            Some(Hover::Unicode(sequence)) => {
                ui.label(text(sequence.as_str()).size(26.0));
                ui.label(ui::bold(format!(":{}:", shortcode(sequence))));
            }
            Some(Hover::Custom(id)) if let Some(emoji) = custom_emoji(id) => {
                if let Some(picture) = &emoji.picture {
                    let (spot, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                    crate::home::paint_live(&ui, spot, picture, 4.0);
                }
                ui.label(ui::bold(format!(":{}:", emoji.name)));
            }
            _ => {
                let tip = match self.tab {
                    Tab::Emoji => t!("picker.favourite_tip"),
                    Tab::Stickers => t!("picker.sticker_tip"),
                };
                ui.label(ui::caption(tip));
            }
        }
    }

    pub fn insert(&mut self, field: &Response, draft: &mut String, sequence: &str) {
        let mut state = TextEdit::load_state(&field.ctx, field.id).unwrap_or_default();
        let end = draft.chars().count();
        let mut cursor = CCursor::new(end);
        if let Some(selection) = self.insert_at.take() {
            let range = selection.as_sorted_char_range();
            cursor = CCursor::new(range.start.0.min(end));
            draft.delete_char_range(cursor.index..range.end.min(end.into()));
        }
        draft.insert_text_at(&mut cursor, sequence, usize::MAX);
        state.cursor.set_char_range(Some(CCursorRange::one(cursor)));
        state.store(&field.ctx, field.id);
        if self.open && docked(&field.ctx) {
            // The panel stays up for more: the next one goes after this one, and the
            // keyboard stays away.
            self.insert_at = Some(CCursorRange::one(cursor));
        } else {
            field.request_focus();
        }
    }
}

/// A line across the panel at `y`.
fn line(ui: &Ui, y: f32) {
    let rect = ui.max_rect();
    ui.painter()
        .hline(rect.x_range(), y, Stroke::new(1.0, ui::colors(ui).border));
}

/// The grid's area, `m.body` tall and inset like the rest of the panel.
fn body<R>(ui: &mut Ui, m: &Metrics, add: impl FnOnce(&mut Ui) -> R) -> R {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(m.width, m.body), Sense::hover());
    let inner = Rect::from_min_max(
        rect.min + Vec2::new(PADDING, ui::SPACE_XS),
        rect.max - Vec2::new(PADDING - 4.0, 0.0),
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    // Tiles are exactly a cell; the emoji fills it without padding.
    child.spacing_mut().item_spacing = Vec2::ZERO;
    child.spacing_mut().button_padding = Vec2::ZERO;
    add(&mut child)
}

/// What the emoji grid shows, taken from the picker for one frame.
struct Saved {
    favourites: Vec<String>,
    recent: Vec<String>,
    query: String,
    tone: usize,
}

impl Saved {
    /// The grid's sections, or one of search results.
    fn groups<'a>(&'a self, scope: &Scope<'a>) -> Vec<Group<'a>> {
        let query = &self.query;
        let packs = scope.packs(false, query);
        let item = |pack: &PackItems<'a>| -> Vec<Item<'a>> {
            (pack.items.iter())
                .map(|e| {
                    if pack.foreign {
                        Item::Foreign(e)
                    } else {
                        Item::Custom(e)
                    }
                })
                .collect()
        };
        if !query.is_empty() {
            let mut items: Vec<Item<'a>> = packs.iter().flat_map(item).collect();
            items.extend(self.found(query));
            return vec![Group {
                section: Section::Found,
                title: None,
                items,
            }];
        }
        let saved = |list: &'a [String]| -> Vec<Item<'a>> {
            (list.iter())
                .map(|sequence| Item::Unicode(sequence, label(sequence)))
                .collect()
        };
        let mut groups = vec![
            Group {
                section: Section::Favourites,
                title: None,
                items: saved(&self.favourites),
            },
            Group {
                section: Section::Recent,
                title: None,
                items: saved(&self.recent),
            },
        ];
        for (at, pack) in packs.iter().enumerate() {
            groups.push(Group {
                section: Section::Pack(at),
                title: Some(pack.title.clone()),
                items: item(pack),
            });
        }
        for group in UNICODE {
            let items = (PICKER.iter())
                .filter(|entry| entry.group == group)
                .map(|entry| Item::Unicode(self.toned(entry), entry.label))
                .collect();
            groups.push(Group {
                section: Section::Unicode(group),
                title: None,
                items,
            });
        }
        groups.retain(|group| !group.items.is_empty());
        groups
    }

    fn toned(&self, entry: &'static super::Entry) -> &'static str {
        let toned = (self.tone.checked_sub(1)).map_or("", |tone| entry.tones[tone]);
        if toned.is_empty() {
            entry.sequence
        } else {
            toned
        }
    }

    fn found(&self, query: &str) -> Vec<Item<'static>> {
        let aliases: HashSet<_> = (SHORTCODES.iter())
            .filter(|(name, _)| name.contains(query))
            .map(|(_, sequence)| *sequence)
            .collect();
        (PICKER.iter())
            .filter(|entry| {
                entry.label.to_lowercase().contains(query)
                    || entry.keywords.contains(query)
                    || aliases.contains(entry.sequence)
                    || entry.sequence == query
            })
            .map(|entry| Item::Unicode(self.toned(entry), entry.label))
            .collect()
    }
}

/// An emoji's name from the picker list, or the emoji itself.
fn label(sequence: &str) -> &str {
    (PICKER.iter())
        .find(|entry| entry.sequence == sequence || entry.tones.contains(&sequence))
        .map_or(sequence, |entry| entry.label)
}

/// The name people type for an emoji: the shortest word (not `+1`), else its picker name.
fn shortcode(sequence: &str) -> String {
    let codes = || SHORTCODES.iter().filter(|(_, emoji)| *emoji == sequence);
    codes()
        .filter(|(name, _)| name.starts_with(|c: char| c.is_ascii_alphabetic()))
        .min_by_key(|(name, _)| name.len())
        .or_else(|| codes().next())
        .map_or_else(
            || label(sequence).to_lowercase().replace(' ', "_"),
            |(name, _)| (*name).to_owned(),
        )
}

fn tile(ui: &mut Ui, sequence: &str, label: &str, cell: f32) -> Response {
    let size = (cell * 0.66).round();
    let response = ui::choice_button(ui, false, text(sequence).size(size), Vec2::splat(cell));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label));
    response
}

/// A server emoji or sticker: its picture, or its name until the picture arrives.
pub fn picture_tile(ui: &mut Ui, emoji: &CustomEmoji, cell: f32) -> Response {
    let label = format!(":{}:", emoji.name);
    let response = match &emoji.picture {
        // It plays while on screen; the grids lay out only the rows in view.
        Some(picture) => {
            let picture = picture.clone();
            let size = Vec2::splat(cell - 8.0);
            let image = egui::Atom::paint(size, move |ui, args| {
                crate::home::paint_live(ui, args.rect, &picture, 0.0);
            });
            ui::choice_button(ui, false, image, Vec2::splat(cell))
        }
        None => ui::choice_button(ui, false, ui::caption(&emoji.name), Vec2::splat(cell)),
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), &label));
    response
}

fn promote(list: &mut Vec<String>, sequence: &str, limit: usize) {
    list.retain(|item| item != sequence);
    list.insert(0, sequence.to_owned());
    list.truncate(limit);
}

pub fn suggestions(
    query: &egui::CompletionQuery<'_>,
    custom: &[CustomEmoji],
) -> Vec<egui::Suggestion> {
    let Some(prefix) = query.word.strip_prefix(':') else {
        return Vec::new();
    };
    if !prefix
        .as_bytes()
        .get(..2)
        .is_some_and(|start| start.iter().all(u8::is_ascii_alphabetic))
        || !prefix.chars().all(shortcode_char)
        || query
            .text
            .chars()
            .nth(query.word_range.end.0)
            .is_some_and(|next| next == ':' || shortcode_char(next))
    {
        return Vec::new();
    }
    let prefix = prefix.to_ascii_lowercase();
    // Server emoji first; they become images when the message is sent.
    let server = custom
        .iter()
        .filter(|e| !e.sticker && e.name.starts_with(&prefix))
        .map(|e| {
            let code = format!(":{}:", e.name);
            egui::Suggestion::new(&code)
                .content(text(format!("{code}  · {}", t!("picker.from_server"))))
        });
    server
        .chain(matching(&prefix).map(|(name, sequence)| {
            egui::Suggestion::new(sequence).content(text(format!("{sequence}  :{name}:")))
        }))
        .take(8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_is_unique_newest_first_and_capped() {
        let mut recent = Vec::new();
        promote(&mut recent, "😀", RECENT_LIMIT);
        promote(&mut recent, "🇳🇱", RECENT_LIMIT);
        promote(&mut recent, "😀", RECENT_LIMIT);
        assert_eq!(recent, ["😀", "🇳🇱"]);
        for index in 0..RECENT_LIMIT {
            promote(&mut recent, &index.to_string(), RECENT_LIMIT);
        }
        assert_eq!(recent.len(), RECENT_LIMIT);
        assert_eq!(recent.last().unwrap(), "0");
    }

    #[test]
    fn hover_bar_names_emoji_by_a_word() {
        assert_eq!(shortcode("😂"), "joy");
        assert_eq!(shortcode("😄"), "smile");
        assert_eq!(shortcode("🇳🇱"), "flag_nl");
    }

    #[test]
    fn favourites_toggle_and_are_kept_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut picker = Picker::new(dir.path());
        picker.toggle_favourite("😀");
        picker.toggle_favourite("🇳🇱");
        picker.toggle_favourite("😀");
        assert_eq!(picker.favourites, ["🇳🇱"]);
        assert_eq!(Picker::new(dir.path()).favourites, ["🇳🇱"]);
    }

    fn nothing() -> Scope<'static> {
        Scope {
            local: None,
            others: Vec::new(),
        }
    }

    fn server(packs: &[(u8, &str)], emoji: &[(&str, bool, u8)]) -> Directory {
        Directory {
            packs: (packs.iter())
                .map(|(id, name)| crate::server::EmojiPack {
                    id: [*id; 32],
                    name: (!name.is_empty()).then(|| (*name).to_owned()),
                })
                .collect(),
            emoji: (emoji.iter())
                .map(|(name, sticker, pack)| CustomEmoji {
                    id: [name.len() as u8 + *pack; 32],
                    name: (*name).into(),
                    sticker: *sticker,
                    pack: [*pack; 32],
                    picture: None,
                })
                .collect(),
            ..Directory::default()
        }
    }

    #[test]
    fn each_pack_is_a_section_and_other_servers_are_marked() {
        let here = server(
            &[(0, ""), (1, "Cats")],
            &[("party", false, 0), ("meow", false, 1), ("hug", true, 1)],
        );
        let there = server(&[(0, "")], &[("wave", false, 0), ("bow", true, 0)]);
        let scope = Scope {
            local: Some(&here),
            others: vec![("Friends", &there)],
        };
        let titles = |sticker| -> Vec<(String, bool, usize)> {
            (scope.packs(sticker, "").into_iter())
                .map(|p| (p.title, p.foreign, p.items.len()))
                .collect()
        };
        // The unnamed pack is the Default one; a server with one pack is shown by its name.
        assert_eq!(
            titles(false),
            [
                (t!("picker.default_pack").to_owned(), false, 1),
                ("Cats".to_owned(), false, 1),
                ("Friends".to_owned(), true, 1),
            ]
        );
        assert_eq!(
            titles(true),
            [
                ("Cats".to_owned(), false, 1),
                ("Friends".to_owned(), true, 1)
            ]
        );
        // The grid has a section per pack between recent and the Unicode ones.
        let saved = Saved {
            favourites: Vec::new(),
            recent: Vec::new(),
            query: String::new(),
            tone: 0,
        };
        let sections: Vec<_> = saved.groups(&scope).iter().map(|g| g.section).collect();
        assert_eq!(
            sections[..4],
            [
                Section::Pack(0),
                Section::Pack(1),
                Section::Pack(2),
                Section::Unicode(0)
            ]
        );
        // Search looks through every pack and every server.
        assert_eq!(scope.packs(false, "w").len(), 2);
        // Picking one of another server is a file; of this server it is text or a sticker.
        let wave = &there.emoji[0];
        assert!(matches!(pick_custom(wave, true), Picked::Foreign(_)));
        assert!(matches!(pick_custom(wave, false), Picked::Text(t) if t == ":wave:"));
        assert!(matches!(
            pick_custom(&here.emoji[2], false),
            Picked::Sticker(..)
        ));
        // A server without packs (older) keeps everything in one.
        let old = server(&[], &[("old", false, 0)]);
        let scope = Scope {
            local: Some(&old),
            others: Vec::new(),
        };
        assert_eq!(scope.packs(false, "").len(), 1);
    }

    #[test]
    fn sections_skip_empty_ones_and_search_has_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut picker = Picker::new(dir.path());
        let saved = |picker: &Picker| Saved {
            favourites: picker.favourites.clone(),
            recent: picker.recent.clone(),
            query: picker.query.clone(),
            tone: 0,
        };
        let sections = |picker: &Picker| -> Vec<Section> {
            saved(picker)
                .groups(&nothing())
                .iter()
                .map(|g| g.section)
                .collect()
        };
        assert_eq!(sections(&picker)[0], Section::Unicode(0));
        picker.toggle_favourite("😀");
        picker.remember("🇳🇱");
        assert_eq!(
            sections(&picker)[..2],
            [Section::Favourites, Section::Recent]
        );
        picker.query = "thumbs".into();
        let saved = saved(&picker);
        let groups = saved.groups(&nothing());
        assert_eq!(groups.len(), 1);
        assert!(
            groups[0]
                .items
                .iter()
                .any(|item| matches!(item, Item::Unicode("👍", _)))
        );
    }
}
