//! Writing in a tile: the text looks as it will while typing (headings are big, bold
//! is bold, the Markdown signs step back), with a toolbar and shortcuts that set it.

use std::sync::Arc;

use eframe::egui::{
    FontId, Galley, Id as UiId, Key, Modifiers, Stroke, TextEdit, TextFormat, Ui, Vec2,
    cache::{ComputerMut, FrameCache},
    text::{CCursor, CCursorRange, LayoutJob},
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::{emoji, ui};

/// How a byte of the text looks.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Look {
    heading: u8,
    /// A sign such as `**` or `#`, rather than text.
    sign: bool,
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: bool,
}

/// Every byte's look. Linear in the text: only text itself takes the nested styles.
fn looks(text: &str) -> Vec<Look> {
    let sign = Look {
        sign: true,
        ..Look::default()
    };
    let mut looks = vec![sign; text.len()];
    let (mut bold, mut italic, mut strike, mut link) = (0, 0, 0, 0);
    let mut heading = 0;
    let mut block = false;
    for (event, range) in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH).into_offset_iter() {
        let leaf = match event {
            Event::Start(tag) => {
                match tag {
                    Tag::Heading { level, .. } => {
                        heading = level as u8;
                        for look in &mut looks[range] {
                            look.heading = heading;
                        }
                    }
                    Tag::Strong => bold += 1,
                    Tag::Emphasis => italic += 1,
                    Tag::Strikethrough => strike += 1,
                    Tag::Link { .. } => link += 1,
                    Tag::CodeBlock(_) => block = true,
                    _ => {}
                }
                continue;
            }
            Event::End(tag) => {
                match tag {
                    TagEnd::Heading(_) => heading = 0,
                    TagEnd::Strong => bold -= 1,
                    TagEnd::Emphasis => italic -= 1,
                    TagEnd::Strikethrough => strike -= 1,
                    TagEnd::Link => link -= 1,
                    TagEnd::CodeBlock => block = false,
                    _ => {}
                }
                continue;
            }
            Event::Text(_) | Event::Html(_) | Event::InlineHtml(_) => range,
            Event::Code(_) => {
                let ticks = text[range.clone()]
                    .bytes()
                    .take_while(|b| *b == b'`')
                    .count();
                range.start + ticks..range.end.saturating_sub(ticks).max(range.start + ticks)
            }
            _ => continue,
        };
        let code = block || text[..leaf.start].ends_with('`') && text[leaf.end..].starts_with('`');
        let look = Look {
            heading,
            sign: false,
            bold: bold > 0,
            italic: italic > 0,
            strike: strike > 0,
            code,
            link: link > 0,
        };
        looks[leaf].fill(look);
    }
    looks
}

#[derive(Default)]
struct Styled;

impl ComputerMut<&str, Arc<Vec<Look>>> for Styled {
    fn compute(&mut self, text: &str) -> Arc<Vec<Look>> {
        Arc::new(looks(text))
    }
}

/// The written text, wrapped to `width`, in the sizes the page shows.
pub(super) fn layout(ui: &Ui, text: &str, width: f32) -> Arc<Galley> {
    let looks = ui.memory_mut(|memory| {
        memory
            .caches
            .cache::<FrameCache<Arc<Vec<Look>>, Styled>>()
            .get(text)
            .clone()
    });
    let c = ui::colors(ui);
    let link = ui.visuals().hyperlink_color;
    let format = |look: Look| {
        let size = match look.heading {
            1 => ui::TEXT_HEADING,
            2 => ui::TEXT_TITLE,
            _ => ui::TEXT_BODY,
        };
        let font = match look.code {
            true => FontId::monospace(ui::TEXT_MONO),
            false => FontId::new(size, emoji::family(look.bold || look.heading > 0)),
        };
        let color = match (look.sign, look.link) {
            (true, _) => c.muted,
            (false, true) => link,
            (false, false) => c.text,
        };
        let mut format = TextFormat::simple(font, color);
        format.italics = look.italic;
        if look.strike {
            format.strikethrough = Stroke::new(1.0, color);
        }
        if look.code {
            format.background = c.field;
        }
        format
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = width;
    let mut start = 0;
    for (at, _) in text.char_indices().skip(1) {
        if looks[at] != looks[start] {
            job.append(&text[start..at], 0.0, format(looks[start]));
            start = at;
        }
    }
    match looks.get(start) {
        Some(look) => job.append(&text[start..], 0.0, format(*look)),
        // An empty field still needs a line's height for its cursor.
        None => job.append("", 0.0, format(Look::default())),
    }
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Mark {
    /// Around the selection; again takes it off.
    Wrap(&'static str, &'static str),
    /// At the start of each selected line, in place of another one; again takes it off.
    Line(&'static str),
    Numbered,
    /// A fenced block around the selection.
    Block,
}

const BOLD: Mark = Mark::Wrap("**", "**");
const ITALIC: Mark = Mark::Wrap("*", "*");
const LINK: Mark = Mark::Wrap("[", "](https://)");

/// A tooltip, read in the current language.
type Tip = fn() -> &'static str;

/// Label, tooltip and what it sets, in groups.
const TOOLS: [&[(&str, Tip, Mark)]; 4] = [
    &[
        ("H1", || t!("home.format_h1"), Mark::Line("# ")),
        ("H2", || t!("home.format_h2"), Mark::Line("## ")),
    ],
    &[
        ("B", || t!("home.format_bold"), BOLD),
        ("I", || t!("home.format_italic"), ITALIC),
        ("S", || t!("home.format_strike"), Mark::Wrap("~~", "~~")),
    ],
    &[
        ("•", || t!("home.format_list"), Mark::Line("- ")),
        ("1.", || t!("home.format_numbered"), Mark::Numbered),
        ("“", || t!("home.format_quote"), Mark::Line("> ")),
    ],
    &[
        ("🔗", || t!("home.format_link"), LINK),
        ("<>", || t!("home.format_code"), Mark::Wrap("`", "`")),
        ("{ }", || t!("home.format_block"), Mark::Block),
    ],
];

fn byte(text: &str, char_at: usize) -> usize {
    text.char_indices()
        .nth(char_at)
        .map_or(text.len(), |(at, _)| at)
}

/// The length of a list number like `12. ` at the start of `line`.
fn number(line: &str) -> Option<usize> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    ((1..=9).contains(&digits) && line[digits..].starts_with(". ")).then_some(digits + 2)
}

/// The line mark `line` starts with.
fn prefix(line: &str) -> &str {
    let hashes = line.bytes().take_while(|b| *b == b'#').count();
    let length = if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        hashes + 1
    } else if ["- ", "* ", "> "].iter().any(|mark| line.starts_with(mark)) {
        2
    } else {
        number(line).unwrap_or(0)
    };
    &line[..length]
}

/// Set `mark` on characters `start..end`; returns the new selection, in characters.
fn apply(text: &mut String, start: usize, end: usize, mark: Mark) -> (usize, usize) {
    let (start, end) = (start.min(end), end.max(start));
    let (from, to) = (byte(text, start), byte(text, end));
    match mark {
        Mark::Wrap(open, close) => {
            let shift = open.chars().count();
            let (before, after) = (&text[..from], &text[to..]);
            // A single * beside the selection may be half of bold's **.
            let half = open == "*" && before.ends_with("**") && !before.ends_with("***");
            if before.ends_with(open) && after.starts_with(close) && !half {
                text.replace_range(to..to + close.len(), "");
                text.replace_range(from - open.len()..from, "");
                (start - shift, end - shift)
            } else {
                text.insert_str(to, close);
                text.insert_str(from, open);
                (start + shift, end + shift)
            }
        }
        Mark::Block => {
            let open = match from == 0 || text[..from].ends_with('\n') {
                true => "```\n",
                false => "\n```\n",
            };
            let close = match text[to..].is_empty() || text[to..].starts_with('\n') {
                true => "\n```",
                false => "\n```\n",
            };
            text.insert_str(to, close);
            text.insert_str(from, open);
            let shift = open.chars().count();
            (start + shift, end + shift)
        }
        Mark::Line(_) | Mark::Numbered => {
            let first = text[..from].rfind('\n').map_or(0, |at| at + 1);
            // A selection that ends at a line's start leaves that line out.
            let reach = match to > from && text[..to].ends_with('\n') {
                true => to - 1,
                false => to,
            };
            let last = text[reach..].find('\n').map_or(text.len(), |at| reach + at);
            let lines: Vec<&str> = text[first..last].split('\n').collect();
            let set = lines.iter().all(|line| match mark {
                Mark::Line(wanted) => prefix(line) == wanted,
                _ => number(line).is_some(),
            });
            let mut written = Vec::with_capacity(lines.len());
            let mut caret = start as isize;
            for (index, line) in lines.iter().enumerate() {
                let old = prefix(line);
                let new = match mark {
                    _ if set => String::new(),
                    Mark::Line(wanted) => wanted.to_owned(),
                    _ => format!("{}. ", index + 1),
                };
                if index == 0 {
                    let column = text[first..from].chars().count() as isize;
                    let kept = (column - old.len() as isize).max(0);
                    caret += new.len() as isize + kept - column;
                }
                written.push(format!("{new}{}", &line[old.len()..]));
            }
            let written = written.join("\n");
            let chosen = if lines.len() > 1 {
                let begin = text[..first].chars().count();
                (begin, begin + written.chars().count())
            } else {
                let caret = caret.max(0) as usize;
                (caret, caret + (end - start).min(written.chars().count()))
            };
            text.replace_range(first..last, &written);
            let all = text.chars().count();
            (chosen.0.min(all), chosen.1.min(all))
        }
    }
}

/// Enter in a list starts its next item; on an empty one it ends the list. `cursor` is
/// just after the new line break; returns where it goes.
fn continue_list(text: &mut String, cursor: usize) -> Option<usize> {
    let at = byte(text, cursor);
    let before = text[..at].strip_suffix('\n')?;
    let first = before.rfind('\n').map_or(0, |at| at + 1);
    let line = &before[first..];
    let indent = line.len() - line.trim_start_matches(' ').len();
    let mark = prefix(&line[indent..]);
    if mark.is_empty() || mark.starts_with('#') {
        return None;
    }
    if line[indent + mark.len()..].trim().is_empty() {
        let taken = text[first..at].chars().count();
        text.replace_range(first..at, "");
        return Some(cursor - taken);
    }
    let next = match number(mark) {
        Some(length) => format!(
            "{}. ",
            mark[..length - 2].parse::<u32>().ok()?.saturating_add(1)
        ),
        None => mark.to_owned(),
    };
    let next = format!("{}{next}", &line[..indent]);
    text.insert_str(at, &next);
    Some(cursor + next.chars().count())
}

fn select(ui: &Ui, field: UiId, (start, end): (usize, usize)) {
    let mut state = TextEdit::load_state(ui.ctx(), field).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(
        CCursor::new(start),
        CCursor::new(end),
    )));
    state.store(ui.ctx(), field);
}

fn set(ui: &Ui, field: UiId, text: &mut String, mark: Mark) {
    let state = TextEdit::load_state(ui.ctx(), field).unwrap_or_default();
    let all = text.chars().count();
    let (start, end) = (state.cursor.char_range()).map_or((all, all), |range| {
        (
            range.primary.index.0.min(all),
            range.secondary.index.0.min(all),
        )
    });
    let selection = apply(text, start, end, mark);
    select(ui, field, selection);
    ui.memory_mut(|memory| memory.request_focus(field));
}

/// Ctrl+B, Ctrl+I and Ctrl+K. Before the field, which would take some of them itself.
pub(super) fn shortcuts(ui: &Ui, field: UiId, text: &mut String) {
    if !ui.memory(|memory| memory.has_focus(field)) {
        return;
    }
    for (key, mark) in [(Key::B, BOLD), (Key::I, ITALIC), (Key::K, LINK)] {
        if ui.input_mut(|input| input.consume_key(Modifiers::COMMAND, key)) {
            set(ui, field, text, mark);
        }
    }
}

/// After the field took an Enter.
pub(super) fn entered(ui: &Ui, field: UiId, text: &mut String) {
    let state = TextEdit::load_state(ui.ctx(), field).unwrap_or_default();
    let Some(range) = state.cursor.char_range() else {
        return;
    };
    if let Some(cursor) = continue_list(text, range.primary.index.0) {
        select(ui, field, (cursor, cursor));
        ui.ctx().request_repaint();
    }
}

/// The formatting buttons, for the tile being written (`None`: greyed out).
pub(super) fn toolbar(ui: &mut Ui, mut writing: Option<(UiId, &mut String)>) {
    let size = Vec2::new(34.0, ui::CONTROL_HEIGHT);
    ui.spacing_mut().item_spacing.x = 2.0;
    ui.spacing_mut().interact_size.y = ui::CONTROL_HEIGHT;
    if writing.is_none() {
        ui.disable();
    }
    for (index, group) in TOOLS.iter().enumerate() {
        if index > 0 {
            ui.add_space(ui::SPACE_M);
        }
        for &(label, tip, mark) in *group {
            let clicked = ui::icon_button(ui, label, tip(), ui::Look::Ghost, size).clicked();
            if clicked && let Some((field, text)) = writing.as_mut() {
                set(ui, *field, text, mark);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[` and `]` around the selection.
    fn marked(source: &str, mark: Mark) -> String {
        let plain = source.replace(['[', ']'], "");
        let start = source.chars().position(|c| c == '[').unwrap();
        let end = source.chars().position(|c| c == ']').unwrap() - 1;
        let mut text = plain;
        let (start, end) = apply(&mut text, start, end, mark);
        let (from, to) = (byte(&text, start), byte(&text, end));
        format!("{}[{}]{}", &text[..from], &text[from..to], &text[to..])
    }

    #[test]
    fn marks_go_on_and_come_off() {
        assert_eq!(marked("Hallo [Welt]", BOLD), "Hallo **[Welt]**");
        assert_eq!(marked("Hallo **[Welt]**", BOLD), "Hallo [Welt]");
        assert_eq!(marked("ä**[ö]**", ITALIC), "ä***[ö]***");
        assert_eq!(marked("a*[b]*", ITALIC), "a[b]");
        assert_eq!(marked("eins\nzw[]ei", Mark::Line("- ")), "eins\n- zw[]ei");
        assert_eq!(marked("- zw[]ei", Mark::Line("- ")), "zw[]ei");
        assert_eq!(marked("- zw[]ei", Mark::Line("# ")), "# zw[]ei");
        assert_eq!(marked("## []", Mark::Line("## ")), "[]");
        assert_eq!(marked("[a\nb]", Mark::Numbered), "[1. a\n2. b]");
        assert_eq!(marked("[1. a\n2. b]", Mark::Numbered), "[a\nb]");
        assert_eq!(marked("[a\n]b", Mark::Line("> ")), "> [a\n]b");
        assert_eq!(marked("x[code]", Mark::Block), "x\n```\n[code]\n```");
    }

    #[test]
    fn every_tool_takes_any_selection() {
        for source in ["", "🦀", "a\n\nb", "- ä\n2. 🦀\n", "#\n> x", "***", "\n"] {
            let all = source.chars().count();
            for &(_, _, mark) in TOOLS.iter().copied().flatten() {
                for start in 0..=all {
                    for end in 0..=all {
                        let mut text = source.to_owned();
                        let (from, to) = apply(&mut text, start, end, mark);
                        let after = text.chars().count();
                        assert!(from <= after && to <= after, "{source:?} {mark:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn enter_continues_and_ends_lists() {
        let entered = |before: &str| {
            let mut text = format!("{before}\n");
            let cursor = text.chars().count();
            continue_list(&mut text, cursor).map(|cursor| (text.clone(), cursor))
        };
        assert_eq!(entered("- a"), Some(("- a\n- ".into(), 6)));
        assert_eq!(entered("9. ä"), Some(("9. ä\n10. ".into(), 9)));
        assert_eq!(entered("  - a"), Some(("  - a\n  - ".into(), 10)));
        assert_eq!(entered("- a\n- "), Some(("- a\n".into(), 4)));
        assert_eq!(entered("text"), None);
        assert_eq!(entered("# title"), None);
    }

    #[test]
    fn the_text_looks_as_it_will() {
        let text = "# Big\n**bold** *it* `code` [go](https://e.com)";
        let looks = looks(text);
        let at = |part: &str| looks[text.find(part).unwrap()];
        assert!(at("# ").sign && at("# ").heading == 1);
        assert!(!at("Big").sign && at("Big").heading == 1);
        assert!(at("**").sign && at("bold").bold && !at("bold").sign);
        assert!(at("it*").italic && !at("it*").bold);
        assert!(at("code").code && !at("code").sign && at("`code").sign);
        assert!(at("go").link && at("](https").sign);
        assert_eq!(super::looks(""), []);
    }
}
