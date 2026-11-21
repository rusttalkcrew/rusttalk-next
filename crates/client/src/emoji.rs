//! Local Unicode emoji artwork and names shared by chat, completion and the picker.

use std::{borrow::Cow, collections::HashMap, ops::Range, sync::LazyLock};

use eframe::egui::{
    self, ColorImage, FontFamily, FontId, FontPriority, GlyphBitmap, GlyphRasterizer,
    GlyphRasterizerRequest, RasterizedGlyph, RichText, vec2,
};
use rusttalk_core::auth::{decode, encode};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::server::{CustomEmoji, Id};

pub mod gif;
pub mod picker;

const PNGS: &[u8] = include_bytes!("../assets/emoji/sprites.bin");
static SPRITES: LazyLock<HashMap<&str, Range<usize>>> = LazyLock::new(|| {
    let mut start = 0;
    include_str!("../assets/emoji/sprites.tsv")
        .lines()
        .map(|line| {
            let (sequence, end) = line.split_once('\t').expect("generated sprite index");
            let end = end.parse().expect("sprite end offset");
            (sequence, std::mem::replace(&mut start, end)..end)
        })
        .collect()
});

fn sprite(sequence: &str) -> Option<&'static [u8]> {
    if !egui::has_emoji_presentation(sequence) && !sequence.ends_with('\u{20e3}') {
        return None;
    }
    let key = if sequence.contains('\u{fe0f}') {
        Cow::Owned(sequence.replace('\u{fe0f}', ""))
    } else {
        Cow::Borrowed(sequence)
    };
    SPRITES.get(key.as_ref()).map(|range| &PNGS[range.clone()])
}

pub fn family(bold: bool) -> FontFamily {
    FontFamily::Name(if bold { "emoji-semibold" } else { "emoji" }.into())
}

pub fn font(size: f32) -> FontId {
    FontId::new(size, family(false))
}

pub fn text(text: impl Into<String>) -> RichText {
    RichText::new(text).family(family(false))
}

pub fn install(ctx: &egui::Context) {
    ctx.add_glyph_rasterizer(
        GlyphRasterizer::new("rusttalk.twemoji", rasterize).with_priority(FontPriority::Highest),
    );
}

fn rasterize(request: &GlyphRasterizerRequest<'_>) -> Option<RasterizedGlyph> {
    let FontFamily::Name(name) = request.family else {
        return None;
    };
    if !matches!(name.as_ref(), "emoji" | "emoji-semibold") {
        return None;
    }
    let png = sprite(request.cluster)?;
    let image = match image::load_from_memory_with_format(png, image::ImageFormat::Png) {
        Ok(image) => image.into_rgba8(),
        Err(error) => {
            tracing::warn!(%error, "reading bundled emoji failed");
            return None;
        }
    };

    let size = request.font_size_px.round().clamp(1.0, 72.0) as u32;
    let image = image::imageops::resize(&image, size, size, image::imageops::FilterType::Triangle);
    Some(RasterizedGlyph {
        bitmap: GlyphBitmap {
            image: ColorImage::from_rgba_unmultiplied([size as usize; 2], image.as_raw()),
            offset_px: vec2(1.0, -(size as f32 * 0.84).round()),
            is_color: true,
        },
        advance_px: size as f32 + 2.0,
    })
}

/// Each emoji is a whole grapheme. Adjacent non-emoji graphemes form one text run.
pub fn runs(text: &str) -> impl Iterator<Item = (&str, bool)> {
    let mut graphemes = text.grapheme_indices(true).peekable();
    std::iter::from_fn(move || {
        let (start, sequence) = graphemes.next()?;
        let emoji = sprite(sequence).is_some();
        if emoji {
            return Some((sequence, true));
        }
        while graphemes
            .next_if(|(_, part)| sprite(part).is_none())
            .is_some()
        {}
        let end = graphemes.peek().map_or(text.len(), |(end, _)| *end);
        Some((&text[start..end], false))
    })
}

/// One to three emoji, Unicode or server, and nothing else: shown large.
pub fn jumbo(text: &str) -> bool {
    let mut count = 0;
    for part in parts(text) {
        let Part::Text(text) = part else {
            count += 1;
            continue;
        };
        for (part, emoji) in runs(text) {
            if emoji {
                count += 1;
            } else if !part.trim().is_empty() {
                return false;
            }
        }
    }
    (1..=3).contains(&count)
}

static SHORTCODES: LazyLock<Vec<(&str, &str)>> = LazyLock::new(|| {
    include_str!("../assets/emoji/shortcodes.tsv")
        .lines()
        .map(|line| line.split_once('\t').expect("generated shortcode"))
        .collect()
});

pub fn shortcode_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+')
}

pub fn matching(prefix: &str) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
    let start = SHORTCODES.partition_point(|(name, _)| *name < prefix);
    SHORTCODES[start..]
        .iter()
        .copied()
        .take_while(move |(name, _)| name.starts_with(prefix))
}

fn lookup(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    matching(&name)
        .next()
        .filter(|(code, _)| *code == name)
        .map(|(_, emoji)| emoji)
}

/// Turn typed `:shortcodes:` into emoji: server emoji become `<:name:id>` tokens, the rest
/// Unicode. Tokens already in the text stay as they are.
pub fn replace<'a>(text: &'a str, custom: &[CustomEmoji]) -> Cow<'a, str> {
    let mut result = String::new();
    let mut changed = false;
    for part in parts(text) {
        match part {
            Part::Text(text) => {
                let replaced = replace_codes(text, custom);
                changed |= matches!(replaced, Cow::Owned(_));
                result.push_str(&replaced);
            }
            Part::Custom { token, .. } => result.push_str(token),
        }
    }
    if changed {
        Cow::Owned(result)
    } else {
        Cow::Borrowed(text)
    }
}

fn replace_codes<'a>(text: &'a str, custom: &[CustomEmoji]) -> Cow<'a, str> {
    let mut result = String::new();
    let mut rest = text;
    while let Some((before, candidate)) = rest.split_once(':') {
        result.push_str(before);
        let end = candidate
            .find(|c| !shortcode_char(c))
            .unwrap_or(candidate.len());
        let name = &candidate[..end];
        let found = candidate[end..].strip_prefix(':').and_then(|after| {
            let server = custom
                .iter()
                .find(|emoji| !emoji.sticker && emoji.name.eq_ignore_ascii_case(name));
            match server {
                Some(emoji) => Some((token(&emoji.name, &emoji.id), after)),
                None => lookup(name).map(|emoji| (emoji.to_owned(), after)),
            }
        });
        if let Some((emoji, after)) = found {
            result.push_str(&emoji);
            rest = after;
        } else {
            result.push(':');
            rest = candidate;
        }
    }
    if rest.len() == text.len() {
        return Cow::Borrowed(text);
    }
    result.push_str(rest);
    Cow::Owned(result)
}

/// How a server emoji travels inside encrypted message text.
pub fn token(name: &str, id: &Id) -> String {
    format!("<:{name}:{}>", encode(id))
}

/// Text, or a server emoji token with its name and ID.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Part<'a> {
    Text(&'a str),
    Custom {
        token: &'a str,
        name: &'a str,
        id: Id,
    },
}

/// Split message text into plain runs and server emoji tokens.
pub fn parts(text: &str) -> Vec<Part<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut search = 0;
    while let Some(at) = text[search..].find("<:").map(|at| search + at) {
        match parse_token(&text[at..]) {
            Some((len, name, id)) => {
                if at > start {
                    out.push(Part::Text(&text[start..at]));
                }
                out.push(Part::Custom {
                    token: &text[at..at + len],
                    name,
                    id,
                });
                start = at + len;
                search = start;
            }
            None => search = at + 2,
        }
    }
    if start < text.len() {
        out.push(Part::Text(&text[start..]));
    }
    out
}

/// `<:name:id>` at the start of `text`: its length, name and ID.
fn parse_token(text: &str) -> Option<(usize, &str, Id)> {
    let rest = text.strip_prefix("<:")?;
    let (name, rest) = rest.split_once(':')?;
    let (id, _) = rest.split_once('>')?;
    if name.is_empty() || name.len() > 32 || !name.chars().all(shortcode_char) {
        return None;
    }
    let len = 2 + name.len() + 1 + id.len() + 1;
    Some((len, name, decode(id).ok()?))
}

/// Text with server emoji tokens shown as `:name:`, for editing, copying and quotes.
pub fn plain(text: &str) -> Cow<'_, str> {
    let parts = parts(text);
    if !parts.iter().any(|part| matches!(part, Part::Custom { .. })) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        parts
            .into_iter()
            .map(|part| match part {
                Part::Text(text) => text.to_owned(),
                Part::Custom { name, .. } => format!(":{name}:"),
            })
            .collect(),
    )
}

pub struct Entry {
    pub group: u8,
    pub sequence: &'static str,
    pub label: &'static str,
    pub keywords: &'static str,
    pub tones: [&'static str; 5],
}

pub static PICKER: LazyLock<Vec<Entry>> = LazyLock::new(|| {
    include_str!("../assets/emoji/picker.tsv")
        .lines()
        .map(|line| {
            let mut columns = line.split('\t');
            let group = columns.next().expect("emoji group");
            let sequence = columns.next().expect("emoji sequence");
            let label = columns.next().expect("emoji label");
            Entry {
                group: group.parse().expect("emoji group number"),
                sequence,
                label,
                keywords: columns.next().unwrap_or_default(),
                tones: std::array::from_fn(|_| columns.next().unwrap_or_default()),
            }
        })
        .collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcode_lookup_and_replacement() {
        assert_eq!(lookup("ROFL"), Some("🤣"));
        assert_eq!(lookup("thumbsup_tone5"), Some("👍🏿"));
        assert_eq!(lookup("smi"), None);
        assert_eq!(
            replace("Hi :smile::+1: :missing: at 12:30", &[]),
            "Hi 😄👍 :missing: at 12:30"
        );
        assert_eq!(replace("::heart: :incomplete", &[]), ":❤️ :incomplete");
        assert_eq!(replace("Plain 😄 text", &[]), "Plain 😄 text");
    }

    #[test]
    fn server_emoji_tokens_round_trip() {
        let party = CustomEmoji {
            id: [7; 32],
            name: "party".into(),
            sticker: false,
            picture: None,
        };
        let smile = CustomEmoji {
            name: "smile".into(),
            ..party.clone()
        };
        let sticker = CustomEmoji {
            name: "big_hug".into(),
            sticker: true,
            ..party.clone()
        };
        let token = token("party", &[7; 32]);
        let sent = replace(
            ":party: :Party: :heart: :big_hug:",
            &[party.clone(), sticker],
        );
        // Stickers are not typed into text.
        assert_eq!(sent, format!("{token} {token} ❤️ :big_hug:"));
        // Server emoji win over Unicode names, and tokens are never replaced again.
        assert_eq!(
            replace(":smile:", std::slice::from_ref(&smile)),
            token.replace("party", "smile")
        );
        assert_eq!(replace(&sent, &[party]), sent);
        assert_eq!(plain(&sent), ":party: :party: ❤️ :big_hug:");
        assert_eq!(
            parts(&format!("a{token}<:bad:x>")),
            [
                Part::Text("a"),
                Part::Custom {
                    token: &token,
                    name: "party",
                    id: [7; 32]
                },
                Part::Text("<:bad:x>"),
            ]
        );
        assert!(jumbo(&format!(" {token}😀 ")));
        assert!(!jumbo(&format!("hi {token}")));
    }

    #[test]
    fn catalog_entries_have_artwork() {
        let sequences = SHORTCODES.iter().map(|(_, sequence)| *sequence).chain(
            PICKER
                .iter()
                .flat_map(|entry| std::iter::once(entry.sequence).chain(entry.tones)),
        );
        for sequence in sequences.filter(|sequence| !sequence.is_empty()) {
            assert!(
                sprite(sequence).is_some(),
                "missing sprite for {sequence:?}"
            );
        }
    }

    #[test]
    fn splits_whole_emoji_and_text_runs() {
        assert_eq!(
            runs("One 👩🏽‍💻 🇳🇱 and 🧑‍🤝‍🧑 1️⃣ 👍🏿!").collect::<Vec<_>>(),
            [
                ("One ", false),
                ("👩🏽‍💻", true),
                (" ", false),
                ("🇳🇱", true),
                (" and ", false),
                ("🧑‍🤝‍🧑", true),
                (" ", false),
                ("1️⃣", true),
                (" ", false),
                ("👍🏿", true),
                ("!", false),
            ]
        );
        assert_eq!(
            runs("❤︎ and 👨‍👩‍👧").collect::<Vec<_>>(),
            [("❤︎ and ", false), ("👨‍👩‍👧", true)]
        );
        assert!(jumbo(" 👩🏽‍💻🇳🇱\n👍🏿 "));
        assert!(!jumbo("😀😀😀😀"));
        assert!(!jumbo("Hi 😀"));
        assert!(!jumbo(" "));
    }
}
