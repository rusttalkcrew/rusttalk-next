//! Highlight visible, bounded snippets off the UI thread. Keep recent results across channel switches.
use crate::ui::{self, Palette};
use egui::{Color32, FontId, Label, RichText, TextFormat, Ui, cache::CacheTrait, text::LayoutJob};
use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock, OnceLock, mpsc},
    thread,
};
use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};

const CAPACITY: usize = 64;
type ResultSlot = Arc<OnceLock<Arc<LayoutJob>>>;
type Work = Box<dyn FnOnce() + Send>;

static WORKER: LazyLock<mpsc::SyncSender<Work>> = LazyLock::new(|| {
    let (worker, jobs) = mpsc::sync_channel::<Work>(CAPACITY);
    thread::spawn(move || {
        for job in jobs {
            job();
        }
    });
    worker
});

#[derive(Default)]
struct Cache {
    entries: VecDeque<(egui::Id, ResultSlot)>,
}
impl CacheTrait for Cache {
    fn update(&mut self) {}
    fn len(&self) -> usize {
        self.entries.len()
    }
}
impl Cache {
    fn get(&mut self, key: egui::Id) -> Option<ResultSlot> {
        let index = self.entries.iter().position(|(k, _)| *k == key)?;
        let entry = self.entries.remove(index)?;
        let slot = entry.1.clone();
        self.entries.push_front(entry);
        Some(slot)
    }
    fn queue(
        &mut self,
        key: egui::Id,
        ctx: egui::Context,
        language: &str,
        text: &str,
        palette: &'static Palette,
    ) {
        let slot = ResultSlot::default();
        let output = slot.clone();
        let (language, text) = (language.to_owned(), text.to_owned());
        if WORKER
            .try_send(Box::new(move || {
                let _ = output.set(Arc::new(highlight(&language, &text, palette)));
                ctx.request_repaint();
            }))
            .is_ok()
        {
            self.entries.push_front((key, slot));
            self.entries.truncate(CAPACITY);
        }
    }
}
fn within_budget(text: &str) -> bool {
    text.len() <= 4096 && text.lines().take(101).count() <= 100
}
pub(super) fn show(ui: &mut Ui, language: &str, text: &str) {
    let palette = ui::colors(ui);
    let key = (!language.is_empty() && within_budget(text))
        .then(|| egui::Id::unique((language, text, palette.id)));
    let slot = key.and_then(|key| ui.memory_mut(|m| m.caches.cache::<Cache>().get(key)));
    if let Some(job) = slot.as_ref().and_then(|slot| slot.get()) {
        ui.add(Label::new(job.clone()).wrap());
    } else {
        let response = ui.add(
            Label::new(
                RichText::new(text)
                    .monospace()
                    .size(ui::TEXT_MONO)
                    .color(palette.text),
            )
            .wrap(),
        );
        if let Some(key) = key
            && slot.is_none()
            && ui.is_rect_visible(response.rect)
        {
            let ctx = ui.ctx().clone();
            ui.memory_mut(|m| {
                m.caches
                    .cache::<Cache>()
                    .queue(key, ctx, language, text, palette)
            });
        }
    }
}
fn readable(color: Color32, palette: &Palette) -> Color32 {
    if ui::contrast(color, palette.field) >= 4.5 {
        color
    } else if ui::contrast(palette.muted, palette.field) >= 4.5 {
        palette.muted
    } else {
        palette.text
    }
}
fn highlight(language: &str, text: &str, palette: &Palette) -> LayoutJob {
    static SYNTAX: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
    static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);
    let theme = &THEMES.themes[if palette.dark {
        "base16-ocean.dark"
    } else {
        "InspiredGitHub"
    }];
    let syntax = SYNTAX
        .find_syntax_by_token(language)
        .unwrap_or_else(|| SYNTAX.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut job = LayoutJob::default();
    for line in text.split_inclusive('\n') {
        let sections = highlighter
            .highlight_line(line, &SYNTAX)
            .unwrap_or_else(|_| {
                vec![(
                    syntect::highlighting::Style {
                        foreground: theme
                            .settings
                            .foreground
                            .unwrap_or(syntect::highlighting::Color::BLACK),
                        ..Default::default()
                    },
                    line,
                )]
            });
        for (style, part) in sections {
            let c = style.foreground;
            job.append(
                part,
                0.,
                TextFormat::simple(
                    FontId::monospace(ui::TEXT_MONO),
                    readable(Color32::from_rgb(c.r, c.g, c.b), palette),
                ),
            );
        }
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn syntax_and_plain_text_meet_contrast_in_all_themes() {
        let code = "// comment\nfn main() { println!(\"hello\"); }\n";
        for palette in ui::DARK_THEMES.into_iter().chain(ui::LIGHT_THEMES) {
            for language in ["rust", "unknown"] {
                let job = highlight(language, code, palette);
                assert_eq!(job.text, code);
                assert!(
                    job.sections
                        .iter()
                        .all(|s| ui::contrast(s.format.color, palette.field) >= 4.5),
                    "{}",
                    palette.id
                );
                if language == "rust" {
                    assert!(
                        job.sections
                            .windows(2)
                            .any(|w| w[0].format.color != w[1].format.color)
                    );
                }
            }
        }
    }
    #[test]
    fn oversized_blocks_use_plain_text() {
        assert!(within_budget(&"x".repeat(4096)));
        assert!(!within_budget(&"x".repeat(4097)));
        assert!(within_budget(&"x\n".repeat(100)));
        assert!(!within_budget(&"x\n".repeat(101)));
    }
}
