use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use eframe::egui::{
    self, Id, PointerButton, Response, ScrollArea, TextBuffer as _, TextEdit, Ui, Vec2,
    text::{CCursor, CCursorRange},
};
use rusttalk_core::client::save_json;

use super::{PICKER, SHORTCODES, matching, shortcode_char, sprite, text};
use crate::ui;

pub const RECENT_FILE: &str = "emoji-recent.json";
const RECENT_LIMIT: usize = 32;
const COLUMNS: usize = 8;
const CELL: f32 = 32.0;
const CATEGORIES: [(&str, Category); 10] = [
    ("Recent", Category::Recent),
    ("Smileys", Category::Unicode(0)),
    ("People", Category::Unicode(1)),
    ("Animals", Category::Unicode(3)),
    ("Food", Category::Unicode(4)),
    ("Travel", Category::Unicode(5)),
    ("Activity", Category::Unicode(6)),
    ("Objects", Category::Unicode(7)),
    ("Symbols", Category::Unicode(8)),
    ("Flags", Category::Unicode(9)),
];

const TONES: [(&str, &str); 6] = [
    ("👋", "Default skin tone"),
    ("👋🏻", "Light skin tone"),
    ("👋🏼", "Medium-light skin tone"),
    ("👋🏽", "Medium skin tone"),
    ("👋🏾", "Medium-dark skin tone"),
    ("👋🏿", "Dark skin tone"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Category {
    Recent,
    Unicode(u8),
}

pub struct Picker {
    data: PathBuf,
    recent: Vec<String>,
    query: String,
    category: Category,
    tone: usize,
    pressed_cursor: Option<CCursorRange>,
    insert_at: Option<CCursorRange>,
}

impl Picker {
    pub fn new(data: &Path) -> Self {
        let stored =
            crate::load_json::<Vec<String>>(&data.join(RECENT_FILE)).unwrap_or_else(|error| {
                tracing::warn!(%error, "reading recent emoji failed");
                Vec::new()
            });
        let mut recent = Vec::new();
        for sequence in stored
            .iter()
            .rev()
            .filter(|sequence| sprite(sequence).is_some())
        {
            promote(&mut recent, sequence);
        }
        Self {
            data: data.to_owned(),
            category: if recent.is_empty() {
                Category::Unicode(0)
            } else {
                Category::Recent
            },
            recent,
            query: String::new(),
            tone: 0,
            pressed_cursor: None,
            insert_at: None,
        }
    }

    pub fn remember(&mut self, sequence: &str) {
        if self.recent.first().is_some_and(|first| first == sequence) {
            return;
        }
        promote(&mut self.recent, sequence);
        if let Err(error) = save_json(&self.data, &self.data.join(RECENT_FILE), &self.recent) {
            tracing::warn!(%error, "saving recent emoji failed");
        }
    }

    pub fn show(&mut self, anchor: &Response, editor: Id) -> Option<String> {
        let cursor = || {
            TextEdit::load_state(&anchor.ctx, editor).and_then(|state| state.cursor.char_range())
        };

        // Opening the picker focuses its search box, which collapses the editor's selection.
        if anchor.ctx.input(|input| input.pointer.primary_pressed()) {
            self.pressed_cursor = if anchor.is_pointer_button_down_on() {
                cursor()
            } else {
                None
            };
        }
        if anchor.clicked() {
            let pressed = self.pressed_cursor.take();
            self.insert_at = if anchor.clicked_by(PointerButton::Primary) {
                pressed.or_else(cursor)
            } else {
                cursor()
            };
            self.query.clear();
        }
        let selected = ui::popup(anchor, |ui| {
            ui.spacing_mut().item_spacing.x = ui::SPACE_XS;
            let search = ui.add(
                ui::field(&mut self.query)
                    .hint_text("Search emoji")
                    .desired_width(ui.available_width()),
            );
            if anchor.clicked() {
                search.request_focus();
            }
            for row in CATEGORIES.chunks(5) {
                ui.horizontal(|ui| {
                    for (name, category) in row {
                        if ui::choice_button(ui, self.category == *category, *name, Vec2::ZERO)
                            .clicked()
                        {
                            self.category = *category;
                            self.query.clear();
                        }
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.label(ui::caption("Skin tone"));
                for (index, (sequence, label)) in TONES.iter().enumerate() {
                    if tile(ui, sequence, label, Some(self.tone == index)).clicked() {
                        self.tone = index;
                    }
                }
            });
            ui.separator();
            let results = self.results();
            if results.is_empty() {
                ui.label(if self.query.trim().is_empty() {
                    "Your recent emoji will appear here."
                } else {
                    "No emoji found."
                });
                return None;
            }
            ScrollArea::vertical()
                .id_salt((self.category, self.query.trim().is_empty()))
                .max_height(216.0)
                .auto_shrink([false, true])
                .show_rows(ui, CELL, results.len().div_ceil(COLUMNS), |ui, rows| {
                    let mut selected = None;
                    for row in rows {
                        ui.push_id(row, |ui| {
                            ui.horizontal(|ui| {
                                for &(sequence, label) in
                                    results.iter().skip(row * COLUMNS).take(COLUMNS)
                                {
                                    if tile(ui, sequence, label, None).clicked() {
                                        selected = Some(sequence.to_owned());
                                    }
                                }
                            });
                        });
                    }
                    if selected.is_some() {
                        ui.close();
                    }
                    selected
                })
                .inner
        })
        .flatten();
        if let Some(sequence) = &selected {
            self.remember(sequence);
        }
        selected
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

    fn results(&self) -> Vec<(&str, &str)> {
        let query = self.query.trim().trim_matches(':').to_lowercase();
        if query.is_empty() && self.category == Category::Recent {
            return self
                .recent
                .iter()
                .map(|sequence| {
                    let label = PICKER
                        .iter()
                        .find(|entry| {
                            entry.sequence == sequence || entry.tones.contains(&sequence.as_str())
                        })
                        .map_or(sequence.as_str(), |entry| entry.label);
                    (sequence.as_str(), label)
                })
                .collect();
        }
        let aliases: HashSet<_> = if query.is_empty() {
            HashSet::new()
        } else {
            SHORTCODES
                .iter()
                .filter(|(name, _)| name.contains(&query))
                .map(|(_, sequence)| *sequence)
                .collect()
        };
        PICKER
            .iter()
            .filter(|entry| {
                if query.is_empty() {
                    self.category == Category::Unicode(entry.group)
                } else {
                    entry.label.to_lowercase().contains(&query)
                        || entry.keywords.contains(&query)
                        || aliases.contains(entry.sequence)
                        || entry.sequence == query
                }
            })
            .map(|entry| {
                let toned = self
                    .tone
                    .checked_sub(1)
                    .map_or("", |tone| entry.tones[tone]);
                (
                    if toned.is_empty() {
                        entry.sequence
                    } else {
                        toned
                    },
                    entry.label,
                )
            })
            .collect()
    }
}

fn tile(ui: &mut Ui, sequence: &str, label: &str, selected: Option<bool>) -> Response {
    let response = ui::choice_button(
        ui,
        selected.unwrap_or_default(),
        text(sequence).size(20.0),
        Vec2::splat(CELL),
    );
    response.widget_info(|| match selected {
        Some(on) => egui::WidgetInfo::selected(egui::Role::RadioButton, ui.is_enabled(), on, label),
        None => egui::WidgetInfo::labeled(egui::Role::Button, ui.is_enabled(), label),
    });
    response.on_hover_text(label)
}

fn promote(recent: &mut Vec<String>, sequence: &str) {
    recent.retain(|item| item != sequence);
    recent.insert(0, sequence.to_owned());
    recent.truncate(RECENT_LIMIT);
}

pub fn suggestions(query: &egui::CompletionQuery<'_>) -> Vec<egui::Suggestion> {
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
    matching(&prefix.to_ascii_lowercase())
        .take(8)
        .map(|(name, sequence)| {
            egui::Suggestion::new(sequence).content(text(format!("{sequence}  :{name}:")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_is_unique_newest_first_and_capped() {
        let mut recent = Vec::new();
        promote(&mut recent, "😀");
        promote(&mut recent, "🇳🇱");
        promote(&mut recent, "😀");
        assert_eq!(recent, ["😀", "🇳🇱"]);
        for index in 0..RECENT_LIMIT {
            promote(&mut recent, &index.to_string());
        }
        assert_eq!(recent.len(), RECENT_LIMIT);
        assert_eq!(recent.last().unwrap(), "0");
    }
}
