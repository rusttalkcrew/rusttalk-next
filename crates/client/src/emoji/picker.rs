//! The picker panel over the composer: tabs for stickers and emoji, a search, a rail of
//! sections, and the hovered emoji's name at the bottom.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use eframe::egui::{
    self, Align, Align2, FontId, Layout, Margin, PointerButton, Rect, Response, ScrollArea, Sense,
    TextBuffer as _, TextEdit, Ui, UiBuilder, Vec2,
    text::{CCursor, CCursorRange},
};
use rusttalk_core::client::save_json;

use super::{
    PICKER, SHORTCODES,
    gif::{self, Gif, Gifs},
    matching, shortcode_char, sprite, text,
};
use crate::{
    server::{CustomEmoji, Directory, Id},
    ui,
};

pub const RECENT_FILE: &str = "emoji-recent.json";
pub const FAVOURITES_FILE: &str = "emoji-favourites.json";
const RECENT_LIMIT: usize = 32;
const FAVOURITES_LIMIT: usize = 64;
const COLUMNS: usize = 9;
const CELL: f32 = 36.0;
const STICKER_COLUMNS: usize = 4;
const STICKER_CELL: f32 = 92.0;
/// The section headers in the emoji grid.
const HEADER: f32 = 28.0;
const RAIL: f32 = 44.0;
const BODY: f32 = 300.0;
const PADDING: f32 = 12.0;
const GIF_COLUMNS: usize = 3;
const GIF_HEIGHT: f32 = 96.0;
/// The rail, the grid and room for its scroll bar.
const WIDTH: f32 = RAIL + COLUMNS as f32 * CELL + 2.0 * PADDING + 4.0;

/// The picker's tabs, each opened by its own composer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Gifs,
    Stickers,
    Emoji,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Gifs => t!("picker.gifs"),
            Tab::Stickers => t!("picker.stickers"),
            Tab::Emoji => t!("picker.emoji"),
        }
    }
}

/// A group of emoji with a header, and its place on the rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Section {
    Favourites,
    Recent,
    /// This server's emoji.
    Server,
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
            Section::Server => t!("picker.server"),
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
            Section::Server => "🏠",
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
    Custom(&'a CustomEmoji),
}

struct Group<'a> {
    section: Section,
    items: Vec<Item<'a>>,
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
    Gif(Gif),
}

pub struct Picker {
    data: PathBuf,
    recent: Vec<String>,
    favourites: Vec<String>,
    open: bool,
    tab: Tab,
    query: String,
    tone: usize,
    /// The section the rail asked for, scrolled to on the next frame.
    jump: Option<Section>,
    /// The section at the top of the grid, lit on the rail.
    current: Option<Section>,
    hover: Option<Hover>,
    focus_search: bool,
    pressed_cursor: Option<CCursorRange>,
    insert_at: Option<CCursorRange>,
    pub gifs: Gifs,
    /// The GIF under the pointer, by title, for the bottom bar.
    gif_hover: Option<String>,
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
        Self {
            data: data.to_owned(),
            recent: load(&data.join(RECENT_FILE)),
            favourites: load(&data.join(FAVOURITES_FILE)),
            open: false,
            tab: Tab::Emoji,
            query: String::new(),
            tone: 0,
            jump: None,
            current: None,
            hover: None,
            focus_search: false,
            pressed_cursor: None,
            insert_at: None,
            gifs: Gifs::default(),
            gif_hover: None,
        }
    }

    pub fn is_open(&self, tab: Tab) -> bool {
        self.open && self.tab == tab
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
            self.open = false;
            return;
        }
        self.insert_at = if response.clicked_by(PointerButton::Primary) {
            pressed.or_else(cursor)
        } else {
            cursor()
        };
        self.open = true;
        self.tab = tab;
        self.query.clear();
        self.hover = None;
        self.gif_hover = None;
        self.focus_search = true;
    }

    /// The panel above `anchor` while open. Returns whether it was open before this
    /// frame, for `button`, and what was picked.
    pub fn show(&mut self, ui: &Ui, anchor: Rect, dir: &Directory) -> (bool, Option<Picked>) {
        let was_open = self.open;
        let mut open = self.open;
        let picked = ui::panel_popup(ui, egui::Id::unique("picker"), anchor, &mut open, |ui| {
            ui.set_width(WIDTH);
            self.panel(ui, dir)
        })
        .flatten();
        self.open = open && picked.is_none();
        if let Some(Picked::Text(sequence)) = &picked {
            self.remember(sequence);
        }
        (was_open, picked)
    }

    fn panel(&mut self, ui: &mut Ui, dir: &Directory) -> Option<Picked> {
        let custom = &dir.emoji;
        ui.spacing_mut().item_spacing = Vec2::splat(ui::SPACE_S);
        egui::Frame::new()
            .inner_margin(Margin {
                bottom: ui::SPACE_S as i8,
                ..Margin::same(PADDING as i8)
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for tab in [Tab::Gifs, Tab::Stickers, Tab::Emoji] {
                        if ui::choice_button(ui, self.tab == tab, tab.label(), Vec2::ZERO).clicked()
                            && self.tab != tab
                        {
                            self.tab = tab;
                            self.query.clear();
                            self.hover = None;
                            self.gif_hover = None;
                            self.focus_search = true;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    let tone = self.tab == Tab::Emoji;
                    let width = ui.available_width() - if tone { CELL + ui::SPACE_S } else { 0.0 };
                    let hint = match self.tab {
                        // KLIPY asks for its name in the search box.
                        Tab::Gifs => t!("picker.search_gifs"),
                        Tab::Stickers => t!("picker.search_stickers"),
                        Tab::Emoji => t!("picker.search"),
                    };
                    let search = ui.add(
                        ui::field(&mut self.query)
                            .hint_text(hint)
                            .desired_width(width),
                    );
                    if std::mem::take(&mut self.focus_search) {
                        search.request_focus();
                    }
                    if search.changed() {
                        self.hover = None;
                        self.gif_hover = None;
                    }
                    if tone {
                        self.tone_button(ui);
                    }
                });
            });
        let c = ui::colors(ui);
        let line = |ui: &Ui, y: f32| {
            let rect = ui.max_rect();
            ui.painter()
                .hline(rect.x_range(), y, egui::Stroke::new(1.0, c.border));
        };
        line(ui, ui.cursor().top());
        let picked = ui
            .allocate_ui_with_layout(
                Vec2::new(WIDTH, BODY),
                Layout::left_to_right(Align::Min),
                |ui| {
                    ui.set_height(BODY);
                    ui.spacing_mut().item_spacing = Vec2::ZERO;
                    // Tiles are exactly a cell; the emoji fills it without padding.
                    ui.spacing_mut().button_padding = Vec2::ZERO;
                    match self.tab {
                        Tab::Emoji => self.emoji(ui, dir),
                        Tab::Stickers => self.stickers(ui, custom),
                        Tab::Gifs => self.gif_tab(ui),
                    }
                },
            )
            .inner;
        line(ui, ui.cursor().top());
        self.bar(ui, custom);
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

    fn emoji(&mut self, ui: &mut Ui, dir: &Directory) -> Option<Picked> {
        let custom = &dir.emoji;
        let saved = Saved {
            favourites: self.favourites.clone(),
            recent: self.recent.clone(),
            query: self.query.trim().trim_matches(':').to_lowercase(),
            tone: self.tone,
        };
        let groups = saved.groups(custom);
        let searching = !self.query.trim().is_empty();
        if !searching {
            self.rail(ui, &groups, dir);
        }
        if groups.iter().all(|group| group.items.is_empty()) {
            ui.add_space(PADDING);
            ui.vertical(|ui| {
                ui.add_space(PADDING);
                ui.label(t!("picker.none_found"));
            });
            return None;
        }
        let mut tops = Vec::with_capacity(groups.len());
        let mut total = 0.0;
        for group in &groups {
            tops.push(total);
            total += group.section.header() + group.items.len().div_ceil(COLUMNS) as f32 * CELL;
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
        ui.add_space(ui::SPACE_XS);
        area.show_viewport(ui, |ui, viewport| {
            let origin = ui.max_rect().min;
            ui.set_width(COLUMNS as f32 * CELL);
            ui.set_height(total);
            for (group, top) in groups.iter().zip(&tops) {
                let header = group.section.header();
                let rows = group.items.len().div_ceil(COLUMNS);
                if top + header + rows as f32 * CELL < viewport.min.y || *top > viewport.max.y {
                    continue;
                }
                if header > 0.0 {
                    let rect = Rect::from_min_size(
                        origin + Vec2::new(4.0, *top),
                        Vec2::new(COLUMNS as f32 * CELL, header),
                    );
                    ui.painter().text(
                        rect.left_center(),
                        Align2::LEFT_CENTER,
                        format!("{}  {}", group.section.icon(), group.section.title()),
                        FontId::new(ui::TEXT_SMALL, ui::bold_family()),
                        ui::colors(ui).muted,
                    );
                }
                let first = ((viewport.min.y - top - header) / CELL).floor().max(0.0) as usize;
                let last =
                    (((viewport.max.y - top - header) / CELL).ceil().max(0.0) as usize).min(rows);
                for row in first..last {
                    let rect = Rect::from_min_size(
                        origin + Vec2::new(0.0, top + header + row as f32 * CELL),
                        Vec2::new(COLUMNS as f32 * CELL, CELL),
                    );
                    let builder = UiBuilder::new()
                        .max_rect(rect)
                        .id_salt((group.section, row))
                        .layout(Layout::left_to_right(Align::Center));
                    ui.scope_builder(builder, |ui| {
                        for item in group.items.iter().skip(row * COLUMNS).take(COLUMNS) {
                            let (response, hover, choice) = match *item {
                                Item::Unicode(sequence, label) => (
                                    tile(ui, sequence, label),
                                    Hover::Unicode(sequence.to_owned()),
                                    Picked::Text(sequence.to_owned()),
                                ),
                                Item::Custom(emoji) => (
                                    picture_tile(ui, emoji, CELL),
                                    Hover::Custom(emoji.id),
                                    Picked::Text(format!(":{}:", emoji.name)),
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
    }

    /// The sections down the left, each scrolling the grid to its header.
    fn rail(&mut self, ui: &mut Ui, groups: &[Group], dir: &Directory) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(RAIL, BODY), Sense::hover());
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::ZERO, ui::colors(ui).side);
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(rect.shrink2(Vec2::new(4.0, 6.0)))
                .layout(Layout::top_down(Align::Center)),
        );
        child.spacing_mut().item_spacing.y = 2.0;
        ScrollArea::vertical()
            .id_salt("picker-rail")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(&mut child, |ui| {
                for group in groups.iter().filter(|group| !group.items.is_empty()) {
                    let section = group.section;
                    let on = self.current == Some(section);
                    let look = if on { ui::Look::Plain } else { ui::Look::Ghost };
                    // The server's icon stands for its emoji, else its first emoji.
                    let first = || {
                        (dir.emoji.iter().find(|emoji| !emoji.sticker))
                            .and_then(|emoji| emoji.picture.as_ref())
                    };
                    let picture = (section == Section::Server)
                        .then(|| dir.icon.as_ref().or_else(first))
                        .flatten();
                    let icon = if picture.is_some() {
                        ""
                    } else {
                        section.icon()
                    };
                    let size = Vec2::splat(ui::ICON_BUTTON);
                    let response = ui::icon_button(ui, icon, section.title(), look, size);
                    if let Some(picture) = picture {
                        let inner =
                            Rect::from_center_size(response.rect.center(), Vec2::splat(22.0));
                        crate::home::paint_hovered(ui, inner, picture, 4.0);
                    }
                    if response.clicked() {
                        self.jump = Some(section);
                    }
                }
            });
    }

    fn stickers(&mut self, ui: &mut Ui, custom: &[CustomEmoji]) -> Option<Picked> {
        let query = self.query.trim().trim_matches(':').to_lowercase();
        let all: Vec<_> = custom.iter().filter(|e| e.sticker).collect();
        let items: Vec<_> = (all.iter())
            .filter(|e| e.name.to_lowercase().contains(&query))
            .collect();
        ui.add_space(PADDING);
        if items.is_empty() {
            ui.vertical(|ui| {
                ui.add_space(PADDING);
                ui.label(if all.is_empty() {
                    t!("picker.no_stickers")
                } else {
                    t!("settings.nothing_found")
                });
            });
            return None;
        }
        let mut picked = None;
        ui.vertical(|ui| {
            ui.add_space(ui::SPACE_XS);
            ScrollArea::vertical()
                .id_salt("picker-stickers")
                .auto_shrink(false)
                .show_rows(
                    ui,
                    STICKER_CELL,
                    items.len().div_ceil(STICKER_COLUMNS),
                    |ui, rows| {
                        for row in rows {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 0.0;
                                let row = items.iter().skip(row * STICKER_COLUMNS);
                                for item in row.take(STICKER_COLUMNS) {
                                    let response = picture_tile(ui, item, STICKER_CELL);
                                    if response.hovered() {
                                        self.hover = Some(Hover::Custom(item.id));
                                    }
                                    if response.clicked() {
                                        picked = Some(Picked::Sticker(item.id, item.name.clone()));
                                    }
                                }
                            });
                        }
                    },
                );
        });
        picked
    }

    /// The hovered emoji large with its `:shortcode:`, or a tip.
    fn bar(&self, ui: &mut Ui, custom: &[CustomEmoji]) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(WIDTH, 48.0), Sense::hover());
        let mut ui = ui.new_child(
            UiBuilder::new()
                .max_rect(rect.shrink2(Vec2::new(PADDING, 6.0)))
                .layout(Layout::left_to_right(Align::Center)),
        );
        let custom_emoji = |id: &Id| custom.iter().find(|emoji| emoji.id == *id);
        match &self.hover {
            Some(Hover::Unicode(sequence)) => {
                ui.label(text(sequence.as_str()).size(26.0));
                ui.label(ui::bold(format!(":{}:", shortcode(sequence))));
            }
            Some(Hover::Custom(id)) if let Some(emoji) = custom_emoji(id) => {
                if let Some(picture) = &emoji.picture {
                    let (spot, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                    crate::home::paint_hovered(&ui, spot, picture, 4.0);
                }
                ui.label(ui::bold(format!(":{}:", emoji.name)));
            }
            _ if self.tab == Tab::Gifs => {
                match &self.gif_hover {
                    Some(title) => ui.label(ui::bold(title)),
                    None => ui.label(ui::caption(t!("picker.powered_by", service = gif::SERVICE))),
                };
            }
            _ => {
                let tip = match self.tab {
                    Tab::Gifs | Tab::Emoji => t!("picker.favourite_tip"),
                    Tab::Stickers => t!("picker.sticker_tip"),
                };
                ui.label(ui::caption(tip));
            }
        }
    }

    /// The GIF tab: the opt-in screen until search is on, then trending or results.
    fn gif_tab(&mut self, ui: &mut Ui) -> Option<Picked> {
        let mut picked = None;
        ui.vertical(|ui| {
            ui.set_width(WIDTH);
            let message = |ui: &mut Ui, text: &str| {
                ui.add_space(PADDING * 2.0);
                ui.vertical_centered(|ui| ui.label(text));
            };
            if gif::KEY.is_none() {
                message(ui, t!("picker.gifs_unavailable"));
                return;
            }
            if !self.gifs.enabled {
                ui.add_space(PADDING * 3.0);
                ui.vertical_centered(|ui| {
                    ui.set_max_width(WIDTH - 4.0 * PADDING);
                    ui.label(ui::title(t!("picker.gifs_off")));
                    ui.add_space(ui::SPACE_XS);
                    ui.label(t!("picker.gifs_off_help", service = gif::SERVICE));
                    ui.add_space(ui::SPACE_M);
                    ui.spacing_mut().button_padding = Vec2::new(ui::SPACE_M, 6.0);
                    if ui::primary(ui, t!("picker.gifs_turn_on")).clicked() {
                        self.gifs.turn_on = true;
                        self.gifs.enabled = true;
                    }
                    ui.add_space(ui::SPACE_S);
                    ui.label(ui::caption(t!("picker.gifs_settings")));
                });
                return;
            }
            if self.gifs.problem {
                ui::error_text(ui, Some(t!("picker.gif_failed")));
            }
            let query = self.query.clone();
            let gifs = match self.gifs.results(ui, &query) {
                gif::Shown::Ready(gifs) => gifs.to_vec(),
                gif::Shown::Failed => {
                    message(ui, t!("picker.gifs_failed"));
                    return;
                }
                gif::Shown::Loading => {
                    ui.add_space(PADDING * 2.0);
                    ui.vertical_centered(|ui| ui.spinner());
                    return;
                }
            };
            if gifs.is_empty() {
                message(ui, t!("picker.none_found"));
                return;
            }
            let cell = Vec2::new(
                (WIDTH - 2.0 * PADDING - 8.0) / GIF_COLUMNS as f32,
                GIF_HEIGHT,
            );
            ui.add_space(ui::SPACE_XS);
            ScrollArea::vertical()
                .id_salt(("picker-gifs", query.trim().is_empty()))
                .auto_shrink(false)
                .show_rows(ui, cell.y, gifs.len().div_ceil(GIF_COLUMNS), |ui, rows| {
                    for row in rows {
                        ui.horizontal(|ui| {
                            ui.add_space(PADDING);
                            for gif in gifs.iter().skip(row * GIF_COLUMNS).take(GIF_COLUMNS) {
                                let response = self.gif_tile(ui, gif, cell);
                                if response.hovered() {
                                    self.gif_hover = Some(gif.title.clone());
                                }
                                if response.clicked() {
                                    picked = Some(Picked::Gif(gif.clone()));
                                }
                            }
                        });
                    }
                });
        });
        picked
    }

    /// A result: its preview playing, cropped to fill the cell.
    fn gif_tile(&mut self, ui: &mut Ui, gif: &Gif, cell: Vec2) -> Response {
        let (rect, response) = ui.allocate_exact_size(cell, Sense::click());
        let inner = rect.shrink(2.0);
        let c = ui::colors(ui);
        ui.painter().rect_filled(inner, ui::RADIUS, c.field);
        if ui.is_rect_visible(rect) {
            match self.gifs.preview(ui, &gif.preview) {
                Some(texture) => {
                    let size = texture.size_vec2();
                    let scale = (inner.size() / size).max_elem();
                    let uv = Rect::from_center_size(
                        egui::pos2(0.5, 0.5),
                        (inner.size() / (size * scale)).min(Vec2::splat(1.0)),
                    );
                    egui::Image::new(&texture)
                        .uv(uv)
                        .corner_radius(ui::RADIUS)
                        .paint_at(ui, inner);
                }
                None => {
                    ui.put(inner, egui::Spinner::new());
                }
            }
        }
        if response.hovered() {
            let stroke = egui::Stroke::new(2.0, ui::accent(ui).text(c.dark));
            ui.painter()
                .rect_stroke(inner, ui::RADIUS, stroke, egui::StrokeKind::Inside);
        }
        ui::focus_ring(ui, &response, ui::RADIUS);
        let label = if gif.title.is_empty() {
            t!("picker.gifs")
        } else {
            &gif.title
        };
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
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
        field.request_focus();
    }
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
    fn groups<'a>(&'a self, custom: &'a [CustomEmoji]) -> Vec<Group<'a>> {
        let query = &self.query;
        let server = |query: &str| -> Vec<Item<'a>> {
            (custom.iter())
                .filter(|e| !e.sticker && e.name.to_lowercase().contains(query))
                .map(Item::Custom)
                .collect()
        };
        if !query.is_empty() {
            let mut items = server(query);
            items.extend(self.found(query));
            return vec![Group {
                section: Section::Found,
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
                items: saved(&self.favourites),
            },
            Group {
                section: Section::Recent,
                items: saved(&self.recent),
            },
            Group {
                section: Section::Server,
                items: server(""),
            },
        ];
        for group in UNICODE {
            let items = (PICKER.iter())
                .filter(|entry| entry.group == group)
                .map(|entry| Item::Unicode(self.toned(entry), entry.label))
                .collect();
            groups.push(Group {
                section: Section::Unicode(group),
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

fn tile(ui: &mut Ui, sequence: &str, label: &str) -> Response {
    let response = ui::choice_button(ui, false, text(sequence).size(24.0), Vec2::splat(CELL));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label));
    response
}

/// A server emoji or sticker: its picture, or its name until the picture arrives.
pub fn picture_tile(ui: &mut Ui, emoji: &CustomEmoji, cell: f32) -> Response {
    let label = format!(":{}:", emoji.name);
    let response = match &emoji.picture {
        Some(picture) => {
            let image = egui::Image::new(&crate::home::frame(ui, picture))
                .fit_to_exact_size(Vec2::splat(cell - 8.0));
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
                .groups(&[])
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
        let groups = saved.groups(&[]);
        assert_eq!(groups.len(), 1);
        assert!(
            groups[0]
                .items
                .iter()
                .any(|item| matches!(item, Item::Unicode("👍", _)))
        );
    }
}
