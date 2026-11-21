//! Local Unicode emoji artwork and names shared by chat, completion and the picker.

use std::{borrow::Cow, collections::HashMap, ops::Range, sync::LazyLock};

use eframe::egui::{
    self, ColorImage, FontFamily, FontId, FontPriority, GlyphBitmap, GlyphRasterizer,
    GlyphRasterizerRequest, RasterizedGlyph, RichText, vec2,
};
use unicode_segmentation::UnicodeSegmentation as _;

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

pub fn jumbo(text: &str) -> bool {
    let mut count = 0;
    for (part, emoji) in runs(text) {
        if emoji {
            count += 1;
        } else if !part.trim().is_empty() {
            return false;
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

pub fn replace(text: &str) -> Cow<'_, str> {
    // Server emoji can supply an additional lookup here when their wire format is defined.
    let mut result = String::new();
    let mut rest = text;
    while let Some((before, candidate)) = rest.split_once(':') {
        result.push_str(before);
        let end = candidate
            .find(|c| !shortcode_char(c))
            .unwrap_or(candidate.len());
        if let Some(after) = candidate[end..].strip_prefix(':')
            && let Some(emoji) = lookup(&candidate[..end])
        {
            result.push_str(emoji);
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
            replace("Hi :smile::+1: :missing: at 12:30"),
            "Hi 😄👍 :missing: at 12:30"
        );
        assert_eq!(replace("::heart: :incomplete"), ":❤️ :incomplete");
        assert_eq!(replace("Plain 😄 text"), "Plain 😄 text");
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
