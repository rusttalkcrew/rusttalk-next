//! What a page document holds: a title, tiles in rows on a grid of twelfths with text and
//! pictures in them, and a banner over it all.

use std::fmt;

use zeroize::Zeroizing;

use super::{LAYOUT, MAX_KEPT, MAX_PAGE_TEXT, TEXT};
use crate::messages::{Attachment, MessageError};

/// A row's width.
pub const GRID: usize = 12;
/// The narrowest tile: a quarter of the page.
pub const MIN_WIDTH: usize = 3;
/// Tiles beside each other, at most.
pub const MAX_COLUMNS: usize = 3;
/// Tiles on one page, at most.
pub const MAX_TILES: usize = 64;
/// Longest title, in Unicode scalar values.
pub const MAX_TITLE: usize = 100;

/// Pictures on one page with the banner: each may keep itself and a thumbnail.
pub const MAX_PICTURES: usize = MAX_KEPT / 2;
/// The narrowest picture, in percent of its tile.
pub const MIN_PICTURE: u8 = 10;
/// A banner's height, in thousandths of the page's width.
pub const MIN_BANNER: u16 = 80;
pub const MAX_BANNER: u16 = 600;
/// The media types a page shows as pictures.
const PICTURE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// Text before, between and after every picture.
const MAX_BLOCKS: usize = 2 * MAX_PICTURES + 1;

/// A tile that is only text.
const MARKDOWN: u8 = 1;
/// A tile of text and pictures.
const MIXED: u8 = 2;
/// The same two, as blocks inside such a tile.
const BLOCK_TEXT: u8 = 1;
const BLOCK_PICTURE: u8 = 2;
/// The optional part after the rows.
const BANNER: u8 = 1;
/// The title's length and the row count.
const PAGE_HEADER: usize = 2 + 1;
/// A row's tile count.
const ROW_HEADER: usize = 1;
/// A tile's width, kind and length.
const TILE_HEADER: usize = 1 + 1 + 2;
const _: () = assert!(MAX_PAGE_TEXT <= u16::MAX as usize && MAX_TILES <= u8::MAX as usize);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

/// A picture on a line of its own.
#[derive(Clone, PartialEq, Eq)]
pub struct Picture {
    pub file: Attachment,
    /// In percent of its tile: `MIN_PICTURE` to 100.
    pub width: u8,
    pub align: Align,
}

// Names and keys stay out of logs and test output.
impl fmt::Debug for Picture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Picture({}%, {:?})", self.width, self.align)
    }
}

/// The picture over the title.
#[derive(Clone, PartialEq, Eq)]
pub struct Banner {
    pub file: Attachment,
    /// In thousandths of the page's width: `MIN_BANNER` to `MAX_BANNER`.
    pub height: u16,
    /// Which part shows where the picture is cut, from left or top (0) to right or
    /// bottom (255).
    pub focus: [u8; 2],
}

impl fmt::Debug for Banner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Banner({}, {:?})", self.height, self.focus)
    }
}

fn is_picture(file: &Attachment) -> bool {
    PICTURE_TYPES.contains(&file.media_type.as_str())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Markdown(String),
    Picture(Picture),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tile {
    /// Text and pictures, top to bottom: 1 to `MAX_BLOCKS` of them.
    Blocks(Vec<Block>),
    /// A kind from a newer app, kept as it is so saving doesn't lose it.
    Unknown { kind: u8, data: Vec<u8> },
}

impl Tile {
    pub fn markdown(text: impl Into<String>) -> Self {
        Self::Blocks(vec![Block::Markdown(text.into())])
    }

    fn blocks(&self) -> &[Block] {
        match self {
            Self::Blocks(blocks) => blocks,
            Self::Unknown { .. } => &[],
        }
    }

    /// The kind byte and how many bytes follow its length.
    fn shape(&self) -> (u8, usize) {
        match self {
            Self::Blocks(blocks) => match blocks.as_slice() {
                [Block::Markdown(text)] => (MARKDOWN, text.len()),
                _ => (MIXED, 1 + blocks.iter().map(block_size).sum::<usize>()),
            },
            Self::Unknown { kind, data } => (*kind, data.len()),
        }
    }

    fn fits(&self) -> bool {
        match self {
            Self::Blocks(blocks) => {
                (1..=MAX_BLOCKS).contains(&blocks.len())
                    && blocks.iter().all(|block| match block {
                        Block::Markdown(_) => true,
                        Block::Picture(picture) => {
                            (MIN_PICTURE..=100).contains(&picture.width)
                                && is_picture(&picture.file)
                        }
                    })
            }
            // A known kind can't pass as an unknown one.
            Self::Unknown { kind, .. } => !matches!(*kind, MARKDOWN | MIXED),
        }
    }

    fn write(&self, out: &mut Vec<u8>) -> Result<(), MessageError> {
        match self {
            Self::Blocks(blocks) => match blocks.as_slice() {
                [Block::Markdown(text)] => out.extend_from_slice(text.as_bytes()),
                _ => {
                    out.push(blocks.len() as u8);
                    for block in blocks {
                        match block {
                            Block::Markdown(text) => {
                                out.push(BLOCK_TEXT);
                                out.extend_from_slice(&(text.len() as u16).to_be_bytes());
                                out.extend_from_slice(text.as_bytes());
                            }
                            Block::Picture(picture) => {
                                out.push(BLOCK_PICTURE);
                                out.push(picture.width);
                                out.push(picture.align as u8);
                                picture.file.encode(out)?;
                            }
                        }
                    }
                }
            },
            Self::Unknown { data, .. } => out.extend_from_slice(data),
        }
        Ok(())
    }

    fn read(kind: u8, data: &[u8]) -> Result<Self, MessageError> {
        let tile = match kind {
            MARKDOWN => Self::markdown(utf8(data)?),
            MIXED => {
                let (&count, mut rest) = data.split_first().ok_or(MessageError::Encoding)?;
                if usize::from(count) > MAX_BLOCKS {
                    return Err(MessageError::Encoding);
                }
                let mut blocks = Vec::new();
                for _ in 0..count {
                    let (&tag, tail) = rest.split_first().ok_or(MessageError::Encoding)?;
                    rest = tail;
                    blocks.push(match tag {
                        BLOCK_TEXT => {
                            let (&length, tail) =
                                rest.split_first_chunk().ok_or(MessageError::Encoding)?;
                            let (text, tail) = tail
                                .split_at_checked(usize::from(u16::from_be_bytes(length)))
                                .ok_or(MessageError::Encoding)?;
                            rest = tail;
                            Block::Markdown(utf8(text)?.to_owned())
                        }
                        BLOCK_PICTURE => {
                            let (&[width, align], mut tail) =
                                rest.split_first_chunk().ok_or(MessageError::Encoding)?;
                            let align = match align {
                                0 => Align::Left,
                                1 => Align::Center,
                                2 => Align::Right,
                                _ => return Err(MessageError::Encoding),
                            };
                            let file = Attachment::decode(&mut tail)
                                .map_err(|_| MessageError::Encoding)?;
                            rest = tail;
                            Block::Picture(Picture { file, width, align })
                        }
                        _ => return Err(MessageError::Encoding),
                    });
                }
                if !rest.is_empty() {
                    return Err(MessageError::Encoding);
                }
                Self::Blocks(blocks)
            }
            _ => Self::Unknown {
                kind,
                data: data.to_vec(),
            },
        };
        if tile.fits() {
            Ok(tile)
        } else {
            Err(MessageError::Encoding)
        }
    }
}

fn reference_size(file: &Attachment) -> usize {
    let blob = 32 + 32 + 4 + 4;
    let thumbnail = if file.thumbnail.is_some() { blob } else { 0 };
    blob + 1 + file.name.len() + 1 + file.media_type.len() + 1 + thumbnail
}

fn block_size(block: &Block) -> usize {
    match block {
        Block::Markdown(text) => 1 + 2 + text.len(),
        Block::Picture(picture) => 1 + 2 + reference_size(&picture.file),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// In twelfths of the page: `MIN_WIDTH` to `GRID`.
    pub width: usize,
    pub tile: Tile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// In reading order: 1 to `MAX_COLUMNS`, together at most `GRID` wide.
    pub tiles: Vec<Placed>,
}

impl Row {
    fn fits(&self) -> bool {
        (1..=MAX_COLUMNS).contains(&self.tiles.len())
            && (self.tiles.iter()).all(|placed| (MIN_WIDTH..=GRID).contains(&placed.width))
            && self.tiles.iter().map(|placed| placed.width).sum::<usize>() <= GRID
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// One line over the tiles, or empty.
    pub title: String,
    pub rows: Vec<Row>,
    pub banner: Option<Banner>,
}

fn utf8(data: &[u8]) -> Result<&str, MessageError> {
    std::str::from_utf8(data).map_err(|_| MessageError::Encoding)
}

fn valid_title(title: &str) -> bool {
    title.chars().count() <= MAX_TITLE && !title.chars().any(char::is_control)
}

impl Layout {
    /// A page of text alone: one full-width tile, or none when empty.
    pub fn text(text: &str) -> Self {
        let tile = Placed {
            width: GRID,
            tile: Tile::markdown(text),
        };
        Self {
            rows: match text.is_empty() {
                true => Vec::new(),
                false => vec![Row { tiles: vec![tile] }],
            },
            ..Self::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.rows.is_empty() && self.banner.is_none()
    }

    /// Every picture's file, the banner's first.
    pub fn pictures(&self) -> impl Iterator<Item = &Attachment> {
        let in_tiles = (self.tiles())
            .flat_map(Tile::blocks)
            .filter_map(|block| match block {
                Block::Picture(picture) => Some(&picture.file),
                Block::Markdown(_) => None,
            });
        (self.banner.iter())
            .map(|banner| &banner.file)
            .chain(in_tiles)
    }

    /// Put each picture's file through `change`, as when uploads get their references.
    pub fn map_pictures(&mut self, mut change: impl FnMut(&mut Attachment)) {
        if let Some(banner) = &mut self.banner {
            change(&mut banner.file);
        }
        for placed in self.rows.iter_mut().flat_map(|row| &mut row.tiles) {
            if let Tile::Blocks(blocks) = &mut placed.tile {
                for block in blocks {
                    if let Block::Picture(picture) = block {
                        change(&mut picture.file);
                    }
                }
            }
        }
    }

    /// The stored files the page uses, pictures and thumbnails, once each.
    pub fn kept(&self) -> Vec<[u8; 32]> {
        let mut kept = Vec::new();
        for file in self.pictures() {
            for blob in std::iter::once(&file.file).chain(&file.thumbnail) {
                if !kept.contains(&blob.id) {
                    kept.push(blob.id);
                }
            }
        }
        kept
    }

    fn fits(&self) -> bool {
        valid_title(&self.title)
            && self.tiles().count() <= MAX_TILES
            && self.rows.iter().all(Row::fits)
            && self.tiles().all(Tile::fits)
            && self.pictures().count() <= MAX_PICTURES
            && (self.banner.as_ref()).is_none_or(|banner| {
                (MIN_BANNER..=MAX_BANNER).contains(&banner.height) && is_picture(&banner.file)
            })
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile> {
        (self.rows.iter()).flat_map(|row| row.tiles.iter().map(|placed| &placed.tile))
    }

    /// The bytes it takes of `MAX_PAGE_TEXT`.
    pub fn size(&self) -> usize {
        let tiles: usize = self.tiles().map(|tile| TILE_HEADER + tile.shape().1).sum();
        let banner =
            (self.banner.as_ref()).map_or(0, |banner| 1 + 2 + 2 + reference_size(&banner.file));
        PAGE_HEADER + self.title.len() + self.rows.len() * ROW_HEADER + tiles + banner
    }

    /// Format 2's plaintext. Every save uses it, text alone too, so the format byte
    /// doesn't tell the server which pages have tiles.
    pub(super) fn encode(&self) -> Result<Zeroizing<Vec<u8>>, MessageError> {
        let size = self.size();
        if size > MAX_PAGE_TEXT {
            return Err(MessageError::PageLimit);
        }
        if !self.fits() {
            return Err(MessageError::Encoding);
        }
        // Full capacity up front, so no plaintext copy is left behind by a reallocation.
        let mut plain = Zeroizing::new(Vec::with_capacity(size));
        plain.extend_from_slice(&(self.title.len() as u16).to_be_bytes());
        plain.extend_from_slice(self.title.as_bytes());
        plain.push(self.rows.len() as u8);
        for row in &self.rows {
            plain.push(row.tiles.len() as u8);
            for placed in &row.tiles {
                let (kind, length) = placed.tile.shape();
                plain.push(placed.width as u8);
                plain.push(kind);
                plain.extend_from_slice(&(length as u16).to_be_bytes());
                placed.tile.write(&mut plain)?;
            }
        }
        if let Some(banner) = &self.banner {
            plain.push(BANNER);
            plain.extend_from_slice(&banner.height.to_be_bytes());
            plain.extend_from_slice(&banner.focus);
            banner.file.encode(&mut plain)?;
        }
        debug_assert_eq!(plain.len(), size);
        Ok(plain)
    }

    /// Read an editor's plaintext. Every count is checked before anything is stored.
    pub(super) fn decode(format: u8, plain: &[u8]) -> Result<Self, MessageError> {
        if plain.len() > MAX_PAGE_TEXT {
            return Err(MessageError::Encoding);
        }
        if format == TEXT {
            return Ok(Self::text(utf8(plain)?));
        }
        if format != LAYOUT {
            return Err(MessageError::Encoding);
        }
        let (&length, rest) = plain.split_first_chunk().ok_or(MessageError::Encoding)?;
        let (title, rest) = rest
            .split_at_checked(usize::from(u16::from_be_bytes(length)))
            .ok_or(MessageError::Encoding)?;
        let title = utf8(title)?;
        if !valid_title(title) {
            return Err(MessageError::Encoding);
        }
        let (&count, mut rest) = rest.split_first().ok_or(MessageError::Encoding)?;
        let mut rows = Vec::new();
        let mut total = 0;
        for _ in 0..count {
            let (&held, tail) = rest.split_first().ok_or(MessageError::Encoding)?;
            rest = tail;
            let held = usize::from(held);
            total += held;
            if !(1..=MAX_COLUMNS).contains(&held) || total > MAX_TILES {
                return Err(MessageError::Encoding);
            }
            let mut tiles = Vec::with_capacity(held);
            for _ in 0..held {
                let (&[width, kind, high, low], tail) =
                    rest.split_first_chunk().ok_or(MessageError::Encoding)?;
                let (data, tail) = tail
                    .split_at_checked(usize::from(u16::from_be_bytes([high, low])))
                    .ok_or(MessageError::Encoding)?;
                rest = tail;
                tiles.push(Placed {
                    width: usize::from(width),
                    tile: Tile::read(kind, data)?,
                });
            }
            let row = Row { tiles };
            if !row.fits() {
                return Err(MessageError::Encoding);
            }
            rows.push(row);
        }
        let mut banner = None;
        if let Some((&tag, tail)) = rest.split_first() {
            let (&[high, low, x, y], mut tail) =
                tail.split_first_chunk().ok_or(MessageError::Encoding)?;
            let file = Attachment::decode(&mut tail).map_err(|_| MessageError::Encoding)?;
            if tag != BANNER || !tail.is_empty() {
                return Err(MessageError::Encoding);
            }
            banner = Some(Banner {
                file,
                height: u16::from_be_bytes([high, low]),
                focus: [x, y],
            });
        }
        let layout = Self {
            title: title.to_owned(),
            rows,
            banner,
        };
        if !layout.fits() {
            return Err(MessageError::Encoding);
        }
        Ok(layout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn markdown(width: usize, text: &str) -> Placed {
        Placed {
            width,
            tile: Tile::markdown(text),
        }
    }

    fn file(id: u8, media_type: &str) -> Attachment {
        Attachment {
            file: crate::messages::BlobRef::for_tests([id; 32]),
            name: "raid.png".into(),
            media_type: media_type.into(),
            thumbnail: Some(crate::messages::BlobRef::for_tests([id + 100; 32])),
        }
    }

    fn picture(id: u8) -> Block {
        Block::Picture(Picture {
            file: file(id, "image/png"),
            width: 60,
            align: Align::Right,
        })
    }

    fn mixed() -> Layout {
        Layout {
            title: "Game night 🎲".into(),
            rows: vec![
                Row {
                    tiles: vec![
                        markdown(5, "# Rules"),
                        markdown(4, "Dates 🦀"),
                        Placed {
                            width: 3,
                            tile: Tile::Unknown {
                                kind: 9,
                                data: vec![0, 255, 7],
                            },
                        },
                    ],
                },
                Row {
                    tiles: vec![
                        markdown(7, "part of the width"),
                        Placed {
                            width: 5,
                            tile: Tile::Blocks(vec![
                                Block::Markdown("Before".into()),
                                picture(1),
                                Block::Markdown("After".into()),
                            ]),
                        },
                    ],
                },
            ],
            banner: Some(Banner {
                file: file(2, "image/gif"),
                height: 250,
                focus: [128, 40],
            }),
        }
    }

    #[test]
    fn pages_list_the_files_they_keep_once() {
        let mut layout = mixed();
        assert_eq!(layout.kept(), [[2; 32], [102; 32], [1; 32], [101; 32]]);
        layout.banner.as_mut().unwrap().file = file(1, "image/png");
        assert_eq!(layout.kept(), [[1; 32], [101; 32]]);
        assert!(Layout::default().kept().is_empty());
    }

    #[test]
    fn pictures_stay_in_their_limits() {
        let page = |blocks| Layout {
            rows: vec![Row {
                tiles: vec![Placed {
                    width: GRID,
                    tile: Tile::Blocks(blocks),
                }],
            }],
            ..Layout::default()
        };
        let many = |count: usize| page((0..count).map(|id| picture(id as u8)).collect());
        assert!(many(MAX_PICTURES).encode().is_ok());
        assert!(many(MAX_PICTURES + 1).encode().is_err());
        assert!(page(Vec::new()).encode().is_err());
        for (media_type, width) in [("text/html", 50), ("image/svg+xml", 50), ("image/png", 9)] {
            let odd = Block::Picture(Picture {
                file: file(1, media_type),
                width,
                align: Align::Left,
            });
            assert!(page(vec![odd]).encode().is_err(), "{media_type} {width}");
        }
        let mut layout = mixed();
        layout.banner.as_mut().unwrap().height = MAX_BANNER + 1;
        assert!(layout.encode().is_err());
    }

    #[test]
    fn layouts_round_trip_with_tiles_this_app_does_not_know() {
        let layout = mixed();
        let plain = layout.encode().unwrap();
        assert_eq!(plain.len(), layout.size());
        assert_eq!(Layout::decode(LAYOUT, &plain), Ok(layout));
    }

    #[test]
    fn the_documented_bytes() {
        let layout = Layout {
            title: "T".into(),
            rows: vec![Row {
                tiles: vec![markdown(6, "hi")],
            }],
            banner: None,
        };
        let plain = layout.encode().unwrap();
        assert_eq!(*plain, [0, 1, b'T', 1, 1, 6, 1, 0, 2, b'h', b'i']);
    }

    #[test]
    fn text_alone_opens_from_both_formats() {
        for text in ["", "# Just text"] {
            let plain = Layout::text(text).encode().unwrap();
            assert_eq!(Layout::decode(LAYOUT, &plain), Ok(Layout::text(text)));
            assert_eq!(
                Layout::decode(TEXT, text.as_bytes()),
                Ok(Layout::text(text))
            );
        }
    }

    #[test]
    fn crafted_layouts_are_refused() {
        let cases: [&[u8]; 15] = [
            &[],
            &[0],
            &[0, 0],
            &[255, 255, 0],
            &[0, 1, 0xff, 0],
            &[0, 1, b'\n', 0],
            &[0, 0, 1],
            &[0, 0, 1, 0],
            &[0, 0, 1, 1, 2, 1, 0, 0],
            &[0, 0, 1, 1, 13, 1, 0, 0],
            &[0, 0, 1, 2, 7, 1, 0, 0, 6, 1, 0, 0],
            &[0, 0, 1, 4, 3, 1, 0, 0, 3, 1, 0, 0, 3, 1, 0, 0, 3, 1, 0, 0],
            &[0, 0, 1, 1, 12, 1, 255, 255],
            &[0, 0, 1, 1, 12, 1, 0, 1, 0xff],
            &[0, 0, 0, 7],
        ];
        for bad in cases {
            assert_eq!(
                Layout::decode(LAYOUT, bad),
                Err(MessageError::Encoding),
                "{bad:?}"
            );
        }
        assert_eq!(Layout::decode(3, &[0, 0, 0]), Err(MessageError::Encoding));
        let mut many = vec![0, 0, (MAX_TILES + 1) as u8];
        for _ in 0..=MAX_TILES {
            many.extend([1, 12, 1, 0, 0]);
        }
        assert_eq!(Layout::decode(LAYOUT, &many), Err(MessageError::Encoding));
    }

    #[test]
    fn every_cut_and_changed_byte_is_handled() {
        let plain = mixed().encode().unwrap();
        // Cut exactly before the banner, it is the same page without one.
        let without = Layout {
            banner: None,
            ..mixed()
        };
        for end in 0..plain.len() {
            let expected = match end == without.size() {
                true => Ok(without.clone()),
                false => Err(MessageError::Encoding),
            };
            assert_eq!(Layout::decode(LAYOUT, &plain[..end]), expected);
        }
        for at in 0..plain.len() {
            for value in 0..=u8::MAX {
                let mut changed = plain.to_vec();
                changed[at] = value;
                // Whatever is accepted must save again unchanged in meaning.
                if let Ok(read) = Layout::decode(LAYOUT, &changed) {
                    let again = read.encode().unwrap();
                    assert_eq!(Layout::decode(LAYOUT, &again), Ok(read));
                }
            }
        }
    }

    #[test]
    fn limits_hold_when_saving() {
        let row = || Row {
            tiles: vec![markdown(GRID, "x")],
        };
        let mut layout = Layout {
            title: "🦀".repeat(MAX_TITLE),
            rows: (0..MAX_TILES).map(|_| row()).collect(),
            banner: None,
        };
        assert!(layout.encode().is_ok());
        layout.rows.push(row());
        assert_eq!(layout.encode().err(), Some(MessageError::Encoding));
        layout.rows.pop();
        layout.title.push('x');
        assert_eq!(layout.encode().err(), Some(MessageError::Encoding));

        let most = MAX_PAGE_TEXT - PAGE_HEADER - ROW_HEADER - TILE_HEADER;
        assert!(Layout::text(&"x".repeat(most)).encode().is_ok());
        let over = Layout::text(&"x".repeat(most + 1));
        assert_eq!(over.encode().err(), Some(MessageError::PageLimit));
    }
}
