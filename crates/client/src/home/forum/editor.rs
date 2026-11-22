//! The Markdown toolbar over a new post: each button marks the selected text, or the
//! line the cursor is on, and a preview shows the post as it will look.

use eframe::egui::{
    Id as UiId, TextEdit, Ui, Vec2,
    text::{CCursor, CCursorRange},
};

use crate::ui;

#[derive(Clone, Copy)]
enum Mark {
    /// Around the selection.
    Wrap(&'static str, &'static str),
    /// At the start of the cursor's line.
    Line(&'static str),
}

/// A tooltip, read in the current language.
type Tip = fn() -> &'static str;

/// Glyph, tooltip and what it adds.
const MARKS: [(&str, Tip, Mark); 9] = [
    ("B", || t!("forum.bold"), Mark::Wrap("**", "**")),
    ("I", || t!("forum.italic"), Mark::Wrap("*", "*")),
    ("S", || t!("forum.strike"), Mark::Wrap("~~", "~~")),
    ("</>", || t!("forum.code"), Mark::Wrap("`", "`")),
    ("🔗", || t!("forum.link"), Mark::Wrap("[", "](https://)")),
    ("H", || t!("forum.heading"), Mark::Line("## ")),
    ("•", || t!("forum.list"), Mark::Line("- ")),
    ("1.", || t!("forum.numbered"), Mark::Line("1. ")),
    ("❝", || t!("forum.quote"), Mark::Line("> ")),
];

fn byte(text: &str, char_at: usize) -> usize {
    text.char_indices()
        .nth(char_at)
        .map_or(text.len(), |(at, _)| at)
}

/// Marks characters `start..end` of `text`; returns the new selection, in characters.
fn apply(text: &mut String, start: usize, end: usize, mark: Mark) -> (usize, usize) {
    let (start, end) = (start.min(end), end.max(start));
    match mark {
        Mark::Wrap(before, after) => {
            text.insert_str(byte(text, end), after);
            text.insert_str(byte(text, start), before);
            let shift = before.chars().count();
            (start + shift, end + shift)
        }
        Mark::Line(prefix) => {
            let line = text[..byte(text, start)].rfind('\n').map_or(0, |at| at + 1);
            text.insert_str(line, prefix);
            let shift = prefix.chars().count();
            (start + shift, end + shift)
        }
    }
}

/// The toolbar row over the text area `field`, whose selection it marks.
/// Returns true when the preview toggle was clicked.
pub(super) fn toolbar(ui: &mut Ui, field: UiId, text: &mut String, preview: bool) -> bool {
    let size = Vec2::splat(ui::CONTROL_HEIGHT);
    let mut toggled = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (glyph, tip, mark) in MARKS {
            let button = ui.add_enabled_ui(!preview, |ui| {
                ui::icon_button(ui, glyph, tip(), ui::Look::Plain, size)
            });
            if button.inner.clicked() {
                let mut state = TextEdit::load_state(ui.ctx(), field).unwrap_or_default();
                let all = text.chars().count();
                let (start, end) = (state.cursor.char_range()).map_or((all, all), |r| {
                    (r.primary.index.0.min(all), r.secondary.index.0.min(all))
                });
                let (start, end) = apply(text, start, end, mark);
                state.cursor.set_char_range(Some(CCursorRange::two(
                    CCursor::new(start),
                    CCursor::new(end),
                )));
                state.store(ui.ctx(), field);
                ui.memory_mut(|m| m.request_focus(field));
            }
        }
        ui.separator();
        let look = if preview {
            ui::Look::On(ui::accent(ui).base)
        } else {
            ui::Look::Plain
        };
        toggled = ui::icon_button(ui, "👁", t!("forum.preview"), look, size).clicked();
    });
    toggled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_wrap_the_selection_or_start_its_line() {
        let mut text = String::from("Hallo Welt");
        assert_eq!(apply(&mut text, 6, 10, MARKS[0].2), (8, 12));
        assert_eq!(text, "Hallo **Welt**");

        let mut text = String::from("eins\nzwei");
        assert_eq!(apply(&mut text, 7, 7, MARKS[6].2), (9, 9));
        assert_eq!(text, "eins\n- zwei");

        // Characters, not bytes, and a selection made backwards.
        let mut text = String::from("äöü");
        apply(&mut text, 2, 1, MARKS[3].2);
        assert_eq!(text, "ä`ö`ü");
    }
}
