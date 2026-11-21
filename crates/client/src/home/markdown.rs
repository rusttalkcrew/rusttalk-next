//! Local presentation of decrypted messages. Draft edits still use the original source.
use std::{
    borrow::Cow,
    ops::Range,
    sync::{Arc, LazyLock},
};

use egui::{
    Color32, FontId, Label, Stroke, TextFormat, Ui,
    cache::{ComputerMut, FrameCache},
    text::LayoutJob,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd, TextMergeWithOffset};
use syntect::{easy::HighlightLines, highlighting::ThemeSet, parsing::SyntaxSet};

use super::{frame, mentions::Message};
use crate::{emoji, server::Directory, ui};

#[derive(Clone, Default)]
struct Style {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    heading: u8,
    link: Option<String>,
}

struct Span {
    text: String,
    style: Style,
    literal: bool,
}

enum Block {
    Line(Vec<Span>),
    Code { language: String, text: String },
}

#[derive(Default)]
pub(super) struct Document {
    blocks: Vec<Block>,
    // Original byte offsets for shortcode and mention substitution in the composer.
    prose: Vec<Range<usize>>,
}

fn web_link(text: &str) -> Option<String> {
    let url = url::Url::parse(text).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.has_host()
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.into())
}

// A server ID or emoji name can itself contain paired Markdown delimiters.
// Mask those delimiters only in prose. Original byte offsets still identify the text.
fn parser_source(source: &str) -> Cow<'_, str> {
    if !source.contains('_') || !source.contains([':', '<']) {
        return Cow::Borrowed(source);
    }
    static TOKEN: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"<@[&a-zA-Z0-9_-]+>|<:[a-zA-Z0-9_+-]+:[a-zA-Z0-9_-]+>|:[a-zA-Z0-9_+-]+:")
            .expect("chat token pattern")
    });
    let mut candidates = TOKEN
        .find_iter(source)
        .filter(|m| m.as_str().contains('_'))
        .map(|m| m.range())
        .peekable();
    if candidates.peek().is_none() {
        return Cow::Borrowed(source);
    }
    let mut masked = Cow::Borrowed(source);
    let mut code = false;
    for (event, range) in Parser::new_ext(source, Options::ENABLE_STRIKETHROUGH).into_offset_iter()
    {
        match event {
            Event::Start(Tag::CodeBlock(_)) => code = true,
            Event::End(TagEnd::CodeBlock) => code = false,
            Event::Text(_) if !code => {
                while let Some(token) = candidates.next_if(|r| r.start < range.end) {
                    if token.start >= range.start {
                        masked
                            .to_mut()
                            .replace_range(token.clone(), &source[token].replace('_', "x"));
                    }
                }
            }
            _ => {}
        }
    }
    masked
}

impl Document {
    pub(super) fn parse(source: &str) -> Self {
        let mut doc = Self::default();
        let mut line = Vec::new();
        let mut style = Style::default();
        let mut styles = Vec::new();
        let mut lists = Vec::new();
        let mut quote = 0usize;
        let mut code = None;
        let flush = |line: &mut Vec<Span>, blocks: &mut Vec<Block>| {
            if !line.is_empty() {
                blocks.push(Block::Line(std::mem::take(line)));
            }
        };
        let mut finder = linkify::LinkFinder::new();
        finder.kinds(&[linkify::LinkKind::Url]);
        let masked = parser_source(source);
        let events = Parser::new_ext(&masked, Options::ENABLE_STRIKETHROUGH)
            .into_offset_iter()
            .map(|(event, range)| {
                let event = match event {
                    Event::Text(text)
                        if matches!(masked, Cow::Owned(_)) && *text == masked[range.clone()] =>
                    {
                        Event::Text(source[range.clone()].into())
                    }
                    other => other,
                };
                (event, range)
            });
        for (event, range) in TextMergeWithOffset::new(events) {
            let literal = !matches!(event, Event::Text(_));
            match event {
                Event::Start(tag) => {
                    styles.push(style.clone());
                    match tag {
                        Tag::Heading { level, .. } => style.heading = level as u8,
                        Tag::Strong if source[range].starts_with("__") => style.underline = true,
                        Tag::Strong => style.bold = true,
                        Tag::Emphasis => style.italic = true,
                        Tag::Strikethrough => style.strike = true,
                        Tag::Link { dest_url, .. } => style.link = web_link(&dest_url),
                        Tag::BlockQuote(_) => quote += 1,
                        Tag::List(start) => lists.push(start),
                        Tag::Item => {
                            flush(&mut line, &mut doc.blocks);
                            let prefix = match lists.last_mut() {
                                Some(Some(n)) => {
                                    let prefix = format!("{n}. ");
                                    *n = n.saturating_add(1);
                                    prefix
                                }
                                _ => "• ".into(),
                            };
                            line.push(Span {
                                text: prefix,
                                style: style.clone(),
                                literal: false,
                            });
                        }
                        Tag::CodeBlock(kind) => {
                            flush(&mut line, &mut doc.blocks);
                            let language = match kind {
                                CodeBlockKind::Fenced(info) => {
                                    info.split_whitespace().next().unwrap_or("").to_owned()
                                }
                                CodeBlockKind::Indented => String::new(),
                            };
                            code = Some((language, String::new()));
                        }
                        _ => {}
                    }
                }
                Event::End(tag) => {
                    style = styles.pop().unwrap_or_default();
                    match tag {
                        TagEnd::CodeBlock => {
                            if let Some((language, text)) = code.take() {
                                doc.blocks.push(Block::Code { language, text });
                            }
                        }
                        TagEnd::BlockQuote(_) => quote -= 1,
                        TagEnd::List(_) => {
                            lists.pop();
                        }
                        TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => {
                            flush(&mut line, &mut doc.blocks)
                        }
                        _ => {}
                    }
                }
                Event::Text(s) if code.is_some() => {
                    if let Some((_, text)) = &mut code {
                        text.push_str(&s);
                    }
                }
                Event::SoftBreak | Event::HardBreak => flush(&mut line, &mut doc.blocks),
                Event::Rule => doc.blocks.push(Block::Line(Vec::new())),
                Event::Text(text)
                | Event::Code(text)
                | Event::Html(text)
                | Event::InlineHtml(text) => {
                    if !literal {
                        doc.prose.push(range);
                    }
                    if line.is_empty() && quote > 0 {
                        line.push(Span {
                            text: "┃ ".into(),
                            style: Style::default(),
                            literal: false,
                        });
                    }
                    if literal || style.link.is_some() {
                        line.push(Span {
                            text: text.into_string(),
                            style: style.clone(),
                            literal,
                        });
                    } else {
                        for piece in finder.spans(&text) {
                            let mut style = style.clone();
                            if piece.kind().is_some() {
                                style.link = web_link(piece.as_str());
                            }
                            line.push(Span {
                                text: piece.as_str().into(),
                                style,
                                literal,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        flush(&mut line, &mut doc.blocks);
        doc
    }

    pub(super) fn prose(&self) -> impl Iterator<Item = &str> {
        self.blocks
            .iter()
            .flat_map(|block| match block {
                Block::Line(spans) => spans.as_slice(),
                _ => &[],
            })
            .filter(|s| !s.literal)
            .map(|s| s.text.as_str())
    }

    pub(super) fn map_source(
        &self,
        source: &str,
        mut replace: impl FnMut(&str) -> String,
    ) -> String {
        let mut result = String::new();
        let mut start = 0;
        for range in &self.prose {
            result.push_str(&source[start..range.start]);
            result.push_str(&replace(&source[range.clone()]));
            start = range.end;
        }
        result.push_str(&source[start..]);
        result
    }

    pub(super) fn plain(&self, mut resolve: impl FnMut(&str) -> String) -> String {
        self.blocks
            .iter()
            .map(|b| match b {
                Block::Code { text, .. } => text.trim_end_matches('\n').into(),
                Block::Line(spans) => spans
                    .iter()
                    .map(|s| {
                        if s.literal {
                            s.text.clone()
                        } else {
                            resolve(&s.text)
                        }
                    })
                    .collect(),
            })
            .collect::<Vec<String>>()
            .join("\n")
    }

    pub(super) fn show(&self, ui: &mut Ui, dir: &Directory, message: &Message, jumbo: bool) {
        for block in &self.blocks {
            match block {
                Block::Line(spans) if spans.is_empty() => {
                    ui.separator();
                }
                Block::Line(spans) => {
                    show_line(ui, dir, message, spans, jumbo);
                }
                Block::Code { language, text } => ui::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        if !language.is_empty() {
                            ui.label(ui::caption(language));
                        }
                        if ui::secondary(ui, t!("home.copy_code")).clicked() {
                            ui.ctx().copy_text(text.clone());
                        }
                    });
                    let dark = ui.visuals().dark_mode;
                    let job = ui.memory_mut(|m| {
                        m.caches
                            .cache::<FrameCache<LayoutJob, Highlighter>>()
                            .get((language.as_str(), text.as_str(), dark))
                            .clone()
                    });
                    ui.add(Label::new(job).wrap());
                }),
            }
        }
    }
}

fn show_line(ui: &mut Ui, dir: &Directory, message: &Message, spans: &[Span], jumbo: bool) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.;
        let mut job = LayoutJob::default();
        let flush = |ui: &mut Ui, job: &mut LayoutJob| {
            if !job.is_empty() {
                ui.add(Label::new(std::mem::take(job)).wrap());
            }
        };
        for s in spans {
            let size = match s.style.heading {
                1 => ui::TEXT_HEADING,
                2 => ui::TEXT_TITLE,
                3.. => ui::TEXT_BODY,
                _ if jumbo => 36.,
                _ => ui::TEXT_BODY,
            };
            let color = ui.visuals().text_color();
            let mut format = TextFormat::simple(
                FontId::new(size, emoji::family(s.style.bold || s.style.heading > 0)),
                color,
            );
            format.italics = s.style.italic;
            if s.style.strike {
                format.strikethrough = Stroke::new(1., color);
            }
            if s.style.underline {
                format.underline = Stroke::new(1., color);
            }
            if s.literal {
                format.font_id = FontId::monospace(ui::TEXT_MONO);
                format.background = ui.visuals().extreme_bg_color;
                job.append(&s.text, 0., format);
            } else if let Some(url) = &s.style.link {
                flush(ui, &mut job);
                ui.hyperlink_to(message.resolve(dir, &s.text), url);
            } else {
                for part in emoji::parts(&s.text) {
                    match part {
                        emoji::Part::Text(text) => {
                            for (text, marked) in message.parts(dir, text) {
                                if marked {
                                    let mut chip = ui::mention_format(ui);
                                    chip.italics = format.italics;
                                    chip.strikethrough = format.strikethrough;
                                    chip.underline = format.underline;
                                    job.append(&text.replace(' ', "\u{a0}"), 0., chip);
                                } else {
                                    job.append(&text, 0., format.clone());
                                }
                            }
                        }
                        emoji::Part::Custom { name, id, .. } => {
                            let label = format!(":{name}:");
                            if let Some(picture) =
                                dir.emoji_by_id(&id).and_then(|e| e.picture.as_ref())
                            {
                                flush(ui, &mut job);
                                let size = if jumbo { 48. } else { size + 6. };
                                ui.add(
                                    egui::Image::new(&frame(ui, picture))
                                        .fit_to_exact_size(egui::Vec2::splat(size))
                                        .alt_text(&label),
                                )
                                .on_hover_text(label);
                            } else {
                                job.append(&label, 0., format.clone());
                            }
                        }
                    }
                }
            }
        }
        flush(ui, &mut job);
    });
}

#[derive(Default)]
struct Parse;
impl ComputerMut<&str, Arc<Document>> for Parse {
    fn compute(&mut self, text: &str) -> Arc<Document> {
        Arc::new(Document::parse(text))
    }
}
pub(super) fn cached(ui: &Ui, text: &str) -> Arc<Document> {
    ui.memory_mut(|m| {
        m.caches
            .cache::<FrameCache<Arc<Document>, Parse>>()
            .get(text)
            .clone()
    })
}

#[derive(Default)]
struct Highlighter;
impl ComputerMut<(&str, &str, bool), LayoutJob> for Highlighter {
    fn compute(&mut self, (language, text, dark): (&str, &str, bool)) -> LayoutJob {
        static SYNTAX: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
        static THEMES: LazyLock<ThemeSet> = LazyLock::new(ThemeSet::load_defaults);
        let theme = &THEMES.themes[if dark {
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
                        Color32::from_rgb(c.r, c.g, c.b),
                    ),
                );
            }
        }
        job
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_keep_visible_text_and_leave_code_literal() {
        let source = "# **Hi** *there*\n\n> ~~old~~ __new__\n\n[guide](https://example.org) and `:wave:`\n\n```rust\n@here :wave:\n```";
        let doc = Document::parse(source);
        assert_eq!(
            doc.plain(str::to_owned),
            "Hi there\n┃ old new\nguide and :wave:\n@here :wave:"
        );
        assert!(
            !doc.prose()
                .any(|s| s.contains("@here") || s.contains(":wave:"))
        );
        assert_eq!(
            doc.map_source(source, |s| s.replace(":wave:", "👋")),
            source
        );
    }

    #[test]
    fn token_delimiters_are_opaque_even_next_to_formatting() {
        let emoji = emoji::token("__party__", &[255; 32]);
        let source = format!("**{emoji}** :__party__: `{emoji}`");
        let doc = Document::parse(&source);
        assert_eq!(
            doc.plain(str::to_owned),
            format!("{emoji} :__party__: {emoji}")
        );
        assert_eq!(
            doc.map_source(&source, |s| s.replace(":__party__:", ":other:")),
            format!(
                "**{}** :other: `{emoji}`",
                emoji.replace(":__party__:", ":other:")
            )
        );
    }

    #[test]
    fn only_web_destinations_are_clickable_and_images_stay_local() {
        let doc = Document::parse(
            "[safe](https://example.org/path) https://example.org/bare [bad](file:///etc) ![alt](https://example.org/image)",
        );
        let Block::Line(spans) = &doc.blocks[0] else {
            panic!()
        };
        let urls: Vec<_> = spans
            .iter()
            .filter_map(|s| s.style.link.as_deref())
            .collect();
        assert_eq!(
            urls,
            ["https://example.org/path", "https://example.org/bare"]
        );
        assert!(doc.plain(str::to_owned).ends_with("bad alt"));
    }

    #[test]
    fn rust_highlights_in_both_modes_and_unknown_code_survives() {
        let code = "fn main() { println!(\"hello\"); }\n";
        for dark in [false, true] {
            let job = Highlighter.compute(("rust", code, dark));
            assert_eq!(job.text, code);
            assert!(
                job.sections
                    .windows(2)
                    .any(|w| w[0].format.color != w[1].format.color)
            );
            assert_eq!(
                Highlighter.compute(("unknown-language", code, dark)).text,
                code
            );
        }
    }
}
