//! A page being edited: its title, rows and tiles, and every way to change them.
//! Nothing is saved until the editor saves the whole page.

use std::path::PathBuf;

use rusttalk_core::{
    client::Attachment,
    pages::{
        Banner, Block, GRID, Layout, MAX_COLUMNS, MAX_PICTURES, MAX_TILES, MAX_TITLE, MIN_WIDTH,
        Picture, Placed, Row, Tile,
    },
};

use super::pictures::{BannerChange, PictureChange};
use crate::server::Id;

/// A new banner's height, in thousandths of the page's width.
const BANNER_HEIGHT: u16 = 250;

/// A tile's name while editing; it stays the same wherever the tile moves.
pub(super) type Key = u64;

#[derive(Clone)]
pub(super) struct Editing {
    pub key: Key,
    pub width: usize,
    pub tile: Tile,
    /// Counts changes to which block is where, so a field never gets the cursor and
    /// undo history of the text that was in its place.
    pub shape: u32,
}

#[derive(Clone)]
pub(super) struct EditRow {
    pub tiles: Vec<Editing>,
}

impl EditRow {
    pub fn room(&self) -> usize {
        GRID.saturating_sub(self.tiles.iter().map(|tile| tile.width).sum())
    }

    pub fn takes_more(&self) -> bool {
        self.tiles.len() < MAX_COLUMNS
    }

    /// Put a tile in. It keeps its width where that fits, else takes the room there is,
    /// else all share the row equally.
    fn insert(&mut self, at: usize, mut tile: Editing) {
        let room = self.room();
        if room >= MIN_WIDTH {
            tile.width = tile.width.min(room);
            self.tiles.insert(at, tile);
            return;
        }
        self.tiles.insert(at, tile);
        let each = GRID / self.tiles.len();
        for tile in &mut self.tiles {
            tile.width = each;
        }
    }
}

/// Where a tile goes, in the rows as they are before it moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// A row of its own, before this row (or after the last).
    NewRow(usize),
    /// Into a row that takes more, before the tile at this place.
    Into(usize, usize),
    /// Change places with this tile, as in a full row.
    Swap(Key),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy)]
pub(super) enum Change {
    Add(Place),
    Write(Option<Key>),
    Move(Key, Place),
    Remove(Key),
    /// Drag a tile's right edge to this width.
    Edge(Key, usize),
    /// Make a tile this wide, taking from any neighbour.
    Width(Key, usize),
    /// Change the picture at this block of a tile.
    Picture(Key, usize, PictureChange),
}

/// One text field: the tile, its shape and the block in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Field {
    pub key: Key,
    pub shape: u32,
    pub block: usize,
}

fn text_of(block: Option<&Block>) -> &str {
    match block {
        Some(Block::Markdown(text)) => text,
        _ => "",
    }
}

fn joined(before: &str, after: &str) -> String {
    let (before, after) = (before.trim_end(), after.trim_start_matches('\n'));
    match before.is_empty() || after.trim().is_empty() {
        true if before.is_empty() => after.to_owned(),
        true => before.to_owned(),
        false => format!("{before}\n\n{after}"),
    }
}

/// For writing: text before, between and after every picture, so there is always
/// somewhere to type.
fn opened(blocks: &[Block]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut text: Option<String> = None;
    for block in blocks {
        match block {
            Block::Markdown(more) => {
                text = Some(match text {
                    Some(before) => joined(&before, more),
                    None => more.clone(),
                });
            }
            Block::Picture(picture) => {
                out.push(Block::Markdown(text.take().unwrap_or_default()));
                out.push(Block::Picture(picture.clone()));
            }
        }
    }
    out.push(Block::Markdown(text.unwrap_or_default()));
    out
}

/// For reading and saving: no empty text, and no trailing space.
fn tidy(blocks: &[Block]) -> Vec<Block> {
    (blocks.iter())
        .filter_map(|block| match block {
            Block::Markdown(text) if text.trim().is_empty() => None,
            // Keep leading indentation: a tile may start with a code block.
            Block::Markdown(text) => Some(Block::Markdown(text.trim_end().to_owned())),
            picture => Some(picture.clone()),
        })
        .collect()
}

/// What takes the keyboard next frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    Title,
    Tile(Key),
}

pub struct Editor {
    /// The version the edit began from.
    pub(super) base: i64,
    pub(super) title: String,
    pub(super) rows: Vec<EditRow>,
    pub(super) banner: Option<Banner>,
    /// The tile open for writing, and the block of it being written.
    pub(super) writing: Option<Key>,
    pub(super) block: usize,
    /// Where the cursor goes with `focus`, in characters; the end if not set.
    pub(super) caret: Option<usize>,
    /// Pictures added in this edit, by the ID that stands in until they are uploaded.
    pub(super) local: Vec<(Id, PathBuf)>,
    /// The next picked picture is the banner.
    pub(super) for_banner: bool,
    pub(super) focus: Option<Focus>,
    next: Key,
}

impl Editor {
    pub(super) fn new(base: i64, layout: &Layout) -> Self {
        let mut editor = Self {
            base,
            title: layout.title.clone(),
            rows: Vec::new(),
            banner: layout.banner.clone(),
            writing: None,
            block: 0,
            caret: None,
            local: Vec::new(),
            for_banner: false,
            focus: None,
            next: 0,
        };
        for row in &layout.rows {
            let tiles = (row.tiles.iter())
                .map(|placed| Editing {
                    key: editor.key(),
                    width: placed.width,
                    tile: placed.tile.clone(),
                    shape: 0,
                })
                .collect();
            editor.rows.push(EditRow { tiles });
        }
        if editor.rows.is_empty() {
            editor.change(Change::Add(Place::NewRow(0)));
        }
        // A new page starts at its title.
        if layout.is_empty() {
            editor.writing = None;
            editor.focus = Some(Focus::Title);
        }
        editor
    }

    fn key(&mut self) -> Key {
        self.next += 1;
        self.next
    }

    /// What saving publishes. Empty text tiles are left out.
    pub(super) fn layout(&self) -> Layout {
        let rows = (self.rows.iter())
            .filter_map(|row| {
                let tiles: Vec<Placed> = (row.tiles.iter())
                    .filter_map(|editing| {
                        let tile = match &editing.tile {
                            Tile::Blocks(blocks) => match tidy(blocks) {
                                blocks if blocks.is_empty() => return None,
                                blocks => Tile::Blocks(blocks),
                            },
                            tile => tile.clone(),
                        };
                        Some(Placed {
                            width: editing.width,
                            tile,
                        })
                    })
                    .collect();
                (!tiles.is_empty()).then_some(Row { tiles })
            })
            .collect();
        // Pasting can bring line breaks into the one-line title.
        let title: String = (self.title.chars())
            .map(|c| if c.is_control() { ' ' } else { c })
            .take(MAX_TITLE)
            .collect();
        Layout {
            title: title.trim().to_owned(),
            rows,
            banner: self.banner.clone(),
        }
    }

    pub(super) fn full(&self) -> bool {
        self.rows.iter().map(|row| row.tiles.len()).sum::<usize>() >= MAX_TILES
    }

    pub(super) fn find(&self, key: Key) -> Option<(usize, usize)> {
        self.rows.iter().enumerate().find_map(|(row, found)| {
            let at = found.tiles.iter().position(|tile| tile.key == key)?;
            Some((row, at))
        })
    }

    /// The text being written.
    pub(super) fn written(&mut self) -> Option<(Field, &mut String)> {
        let key = self.writing?;
        let (row, at) = self.find(key)?;
        let editing = &mut self.rows[row].tiles[at];
        let field = Field {
            key,
            shape: editing.shape,
            block: self.block,
        };
        let Tile::Blocks(blocks) = &mut editing.tile else {
            return None;
        };
        match blocks.get_mut(self.block)? {
            Block::Markdown(text) => Some((field, text)),
            Block::Picture(_) => None,
        }
    }

    fn write(&mut self, key: Option<Key>) {
        self.focus = key.map(Focus::Tile);
        self.caret = None;
        if key == self.writing {
            return;
        }
        if let Some((row, at)) = self.writing.and_then(|old| self.find(old)) {
            let editing = &mut self.rows[row].tiles[at];
            if let Tile::Blocks(blocks) = &editing.tile {
                let mut closed = tidy(blocks);
                if closed.is_empty() {
                    closed.push(Block::Markdown(String::new()));
                }
                editing.tile = Tile::Blocks(closed);
                editing.shape += 1;
            }
        }
        self.writing = key;
        self.block = 0;
        if let Some((row, at)) = key.and_then(|key| self.find(key)) {
            let editing = &mut self.rows[row].tiles[at];
            if let Tile::Blocks(blocks) = &editing.tile {
                let open = opened(blocks);
                self.block = open.len() - 1;
                editing.tile = Tile::Blocks(open);
                editing.shape += 1;
            }
        }
    }

    fn picture_count(&self) -> usize {
        let in_tiles = (self.rows.iter())
            .flat_map(|row| &row.tiles)
            .filter_map(|editing| match &editing.tile {
                Tile::Blocks(blocks) => Some(blocks),
                Tile::Unknown { .. } => None,
            })
            .flatten()
            .filter(|block| matches!(block, Block::Picture(_)));
        in_tiles.count() + usize::from(self.banner.is_some())
    }

    pub(super) fn takes_picture(&self) -> bool {
        self.picture_count() < MAX_PICTURES || (self.for_banner && self.banner.is_some())
    }

    pub(super) fn set_banner(&mut self, file: Attachment) {
        self.banner = Some(Banner {
            file,
            height: self.banner.as_ref().map_or(BANNER_HEIGHT, |old| old.height),
            focus: [128, 128],
        });
    }

    pub(super) fn change_banner(&mut self, change: BannerChange) {
        match change {
            BannerChange::Remove => self.banner = None,
            BannerChange::Replace => self.for_banner = true,
            BannerChange::Focus(focus) => {
                if let Some(banner) = &mut self.banner {
                    banner.focus = focus;
                }
            }
            BannerChange::Height(height) => {
                if let Some(banner) = &mut self.banner {
                    banner.height = height;
                }
            }
        }
    }

    /// Put a picture at the cursor, between lines, or in a tile of its own when no
    /// text is being written. Returns whether it went in.
    pub(super) fn insert_picture(&mut self, file: Attachment, cursor: Option<usize>) -> bool {
        if self.picture_count() >= MAX_PICTURES {
            return false;
        }
        let picture = Block::Picture(Picture {
            file,
            width: 100,
            align: Default::default(),
        });
        let block = self.block;
        if let Some((field, text)) = self.written() {
            let mut split = cursor
                .and_then(|cursor| text.char_indices().nth(cursor))
                .map_or(text.len(), |(at, _)| at);
            // Mid-line it goes after that line.
            if split > 0 && !text[..split].ends_with('\n') {
                split += text[split..].find('\n').unwrap_or(text.len() - split);
            }
            let after = text[split..].trim_start_matches('\n').to_owned();
            *text = text[..split].trim_end().to_owned();
            if let Some((row, at)) = self.find(field.key) {
                let editing = &mut self.rows[row].tiles[at];
                if let Tile::Blocks(blocks) = &mut editing.tile {
                    blocks.splice(block + 1..block + 1, [picture, Block::Markdown(after)]);
                    editing.shape += 1;
                }
            }
            self.block = block + 2;
            self.caret = Some(0);
            self.focus = Some(Focus::Tile(field.key));
            return true;
        }
        // A new page's empty tile becomes the picture's.
        let empty = match self.rows.as_mut_slice() {
            [row] if row.tiles.len() == 1 => Some(&mut row.tiles[0]).filter(
                |editing| matches!(&editing.tile, Tile::Blocks(blocks) if tidy(blocks).is_empty()),
            ),
            _ => None,
        };
        if let Some(editing) = empty {
            editing.tile = Tile::Blocks(vec![picture]);
            editing.shape += 1;
            return true;
        }
        if self.full() {
            return false;
        }
        let key = self.key();
        self.rows.push(EditRow {
            tiles: vec![Editing {
                key,
                width: GRID,
                tile: Tile::Blocks(vec![picture]),
                shape: 0,
            }],
        });
        true
    }

    /// Change the picture at `block` of a tile open for writing.
    fn change_picture(&mut self, key: Key, block: usize, change: PictureChange) {
        let Some((row, at)) = self.find(key) else {
            return;
        };
        let editing = &mut self.rows[row].tiles[at];
        let Tile::Blocks(blocks) = &mut editing.tile else {
            return;
        };
        let Some(Block::Picture(picture)) = blocks.get_mut(block) else {
            return;
        };
        match change {
            PictureChange::Width(width) => {
                picture.width = width;
                return;
            }
            PictureChange::Align(align) => {
                picture.align = align;
                return;
            }
            _ => {}
        }
        let Some(above) = block.checked_sub(1) else {
            return;
        };
        let (before, after) = (
            text_of(blocks.get(above)).trim_end().to_owned(),
            text_of(blocks.get(block + 1))
                .trim_start_matches('\n')
                .to_owned(),
        );
        match change {
            PictureChange::Remove => {
                let merged = Block::Markdown(joined(&before, &after));
                blocks.splice(above..(block + 2).min(blocks.len()), [merged]);
            }
            PictureChange::Up => {
                if before.is_empty() {
                    if block < 2 {
                        return;
                    }
                    blocks.swap(block - 2, block);
                } else {
                    // Past the paragraph above it.
                    let split = before.rfind("\n\n").map_or(0, |at| at + 2);
                    let (stays, passed) = before.split_at(split);
                    blocks[above] = Block::Markdown(stays.trim_end().to_owned());
                    if let Some(below) = blocks.get_mut(block + 1) {
                        *below = Block::Markdown(joined(passed, &after));
                    }
                }
            }
            PictureChange::Down => {
                if after.trim().is_empty() {
                    if block + 2 >= blocks.len() {
                        return;
                    }
                    blocks.swap(block, block + 2);
                } else {
                    let end = after.find("\n\n").unwrap_or(after.len());
                    let (passed, stays) = after.split_at(end);
                    blocks[above] = Block::Markdown(joined(&before, passed));
                    blocks[block + 1] = Block::Markdown(stays.trim_start_matches('\n').to_owned());
                }
            }
            PictureChange::Width(_) | PictureChange::Align(_) => {}
        }
        self.block = above.min(blocks.len().saturating_sub(1));
        editing.shape += 1;
    }

    /// The pictures this save uploads: those added here that the page still shows.
    pub(super) fn uploads(&self, layout: &Layout) -> Vec<(Id, PathBuf)> {
        (self.local.iter())
            .filter(|(id, _)| layout.pictures().any(|file| file.file.id == *id))
            .cloned()
            .collect()
    }

    /// The edit is over: pasted pictures' temporary copies go.
    pub(crate) fn close(self) {
        for (_, path) in &self.local {
            crate::files::discard(path);
        }
    }

    pub(super) fn change(&mut self, change: Change) {
        match change {
            Change::Add(place) => self.add(place),
            Change::Write(key) => self.write(key),
            Change::Picture(key, block, change) => self.change_picture(key, block, change),
            Change::Move(key, place) => self.place(key, place),
            Change::Remove(key) => self.remove(key),
            Change::Edge(key, width) => self.edge(key, width),
            Change::Width(key, width) => self.set_width(key, width),
        }
    }

    fn add(&mut self, place: Place) {
        if self.full() {
            return;
        }
        let key = self.key();
        let new = Editing {
            key,
            width: GRID,
            tile: Tile::markdown(""),
            shape: 0,
        };
        match place {
            Place::NewRow(at) if at <= self.rows.len() => {
                self.rows.insert(at, EditRow { tiles: vec![new] });
            }
            Place::Into(row, at) => match self.rows.get_mut(row) {
                Some(row) if row.takes_more() && at <= row.tiles.len() => row.insert(at, new),
                _ => return,
            },
            _ => return,
        }
        self.change(Change::Write(Some(key)));
    }

    /// Take a tile away; a row left empty goes with it.
    fn remove(&mut self, key: Key) {
        let Some((row, at)) = self.find(key) else {
            return;
        };
        self.rows[row].tiles.remove(at);
        if self.rows[row].tiles.is_empty() {
            self.rows.remove(row);
        }
        if self.writing == Some(key) {
            self.change(Change::Write(None));
        }
    }

    /// The right edge is the border to the next tile: growing takes the free room, then
    /// the tiles after it; shrinking gives to the next one, or frees room at the end.
    fn edge(&mut self, key: Key, wanted: usize) {
        let Some((row, at)) = self.find(key) else {
            return;
        };
        let room = self.rows[row].room();
        let (before, after) = self.rows[row].tiles.split_at_mut(at + 1);
        let tile = &mut before[at];
        let wanted = wanted.max(MIN_WIDTH);
        if wanted < tile.width {
            let given = tile.width - wanted;
            tile.width = wanted;
            if let Some(next) = after.first_mut() {
                next.width += given;
            }
            return;
        }
        let mut more = wanted - tile.width;
        let free = more.min(room);
        tile.width += free;
        more -= free;
        for next in after {
            let taken = more.min(next.width - MIN_WIDTH);
            next.width -= taken;
            tile.width += taken;
            more -= taken;
        }
    }

    /// Growing takes the free room, then the widest neighbour; shrinking frees room.
    fn set_width(&mut self, key: Key, wanted: usize) {
        let Some((row, at)) = self.find(key) else {
            return;
        };
        let wanted = wanted.clamp(MIN_WIDTH, GRID);
        let mut room = self.rows[row].room();
        let tiles = &mut self.rows[row].tiles;
        if wanted <= tiles[at].width {
            tiles[at].width = wanted;
            return;
        }
        while tiles[at].width < wanted {
            if room > 0 {
                room -= 1;
            } else {
                let Some(widest) = (0..tiles.len())
                    .filter(|&other| other != at && tiles[other].width > MIN_WIDTH)
                    .max_by_key(|&other| tiles[other].width)
                else {
                    return;
                };
                tiles[widest].width -= 1;
            }
            tiles[at].width += 1;
        }
    }

    fn place(&mut self, key: Key, place: Place) {
        let Some((from, at)) = self.find(key) else {
            return;
        };
        match place {
            Place::Swap(other) => {
                let Some((to, there)) = self.find(other) else {
                    return;
                };
                let mut first = self.rows[from].tiles[at].clone();
                let mut second = self.rows[to].tiles[there].clone();
                // The places keep their widths, so both rows still fit.
                std::mem::swap(&mut first.width, &mut second.width);
                self.rows[from].tiles[at] = second;
                self.rows[to].tiles[there] = first;
            }
            Place::Into(to, mut before) => {
                let Some(target) = self.rows.get(to) else {
                    return;
                };
                if before > target.tiles.len() {
                    return;
                }
                if to == from {
                    before -= usize::from(at < before);
                    let tile = self.rows[from].tiles.remove(at);
                    self.rows[from].tiles.insert(before, tile);
                    return;
                }
                if !target.takes_more() {
                    return;
                }
                let tile = self.rows[from].tiles.remove(at);
                self.rows[to].insert(before, tile);
                if self.rows[from].tiles.is_empty() {
                    self.rows.remove(from);
                }
            }
            Place::NewRow(mut before) => {
                if before > self.rows.len() {
                    return;
                }
                let mut tile = self.rows[from].tiles.remove(at);
                if self.rows[from].tiles.is_empty() {
                    self.rows.remove(from);
                    before -= usize::from(from < before);
                } else {
                    // Leaving its neighbours, it gets the whole width.
                    tile.width = GRID;
                }
                self.rows.insert(before, EditRow { tiles: vec![tile] });
            }
        }
    }

    /// Where one step from the menu takes a tile; `None` at the edge. Up and down join a
    /// row that takes more and change places in a full one.
    pub(super) fn step(&self, key: Key, step: Step) -> Option<Place> {
        let (row, at) = self.find(key)?;
        let tiles = &self.rows[row].tiles;
        let other = match step {
            Step::Left => return Some(Place::Into(row, at.checked_sub(1)?)),
            Step::Right => return (at + 1 < tiles.len()).then_some(Place::Into(row, at + 2)),
            Step::Up if row > 0 => row - 1,
            Step::Up => return (tiles.len() > 1).then_some(Place::NewRow(0)),
            Step::Down if row + 1 < self.rows.len() => row + 1,
            Step::Down => return (tiles.len() > 1).then_some(Place::NewRow(row + 1)),
        };
        let target = &self.rows[other];
        // Two rows of one tile each trade places, as in a plain list.
        if tiles.len() == 1 && target.tiles.len() == 1 {
            return Some(Place::NewRow(match step {
                Step::Up => other,
                _ => other + 1,
            }));
        }
        let last = target.tiles.len();
        Some(if target.takes_more() {
            Place::Into(other, at.min(last))
        } else {
            Place::Swap(target.tiles.get(at.min(last - 1))?.key)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows as tiles of one letter and their width.
    type Rows<'a> = &'a [&'a [(&'a str, usize)]];

    fn editor(rows: Rows) -> Editor {
        let rows = (rows.iter())
            .map(|tiles| Row {
                tiles: (tiles.iter())
                    .map(|&(name, width)| Placed {
                        width,
                        tile: Tile::markdown(name),
                    })
                    .collect(),
            })
            .collect();
        Editor::new(
            1,
            &Layout {
                rows,
                ..Layout::default()
            },
        )
    }

    fn shown(editor: &Editor) -> String {
        (editor.layout().rows.iter())
            .map(|row| {
                (row.tiles.iter())
                    .map(|placed| match &placed.tile {
                        Tile::Blocks(blocks) => {
                            format!("{}{}", text_of(blocks.first()), placed.width)
                        }
                        Tile::Unknown { .. } => "?".into(),
                    })
                    .collect::<Vec<_>>()
                    .join("+")
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn key(editor: &Editor, name: &str) -> Key {
        (editor.rows.iter())
            .flat_map(|row| &row.tiles)
            .find(|tile| tile.tile == Tile::markdown(name))
            .unwrap()
            .key
    }

    fn stored(id: u8) -> Attachment {
        Attachment {
            file: rusttalk_core::client::BlobRef::for_tests([id; 32]),
            name: "raid.png".into(),
            media_type: "image/png".into(),
            thumbnail: None,
        }
    }

    /// The first tile's blocks, a picture as its ID in brackets.
    fn blocks(editor: &Editor) -> String {
        let layout = editor.layout();
        let Some(Tile::Blocks(blocks)) = layout.tiles().next() else {
            return String::new();
        };
        (blocks.iter())
            .map(|block| match block {
                Block::Markdown(text) => text.clone(),
                Block::Picture(picture) => format!("[{}]", picture.file.file.id[0]),
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn a_picture_goes_between_the_lines_at_the_cursor() {
        let text = "One\n\nTwo\n\nThree";
        for (cursor, expected) in [
            (Some(0), "[1]|One\n\nTwo\n\nThree"),
            (Some(2), "One|[1]|Two\n\nThree"),
            (Some(5), "One|[1]|Two\n\nThree"),
            (None, "One\n\nTwo\n\nThree|[1]"),
        ] {
            let mut page = editor(&[&[(text, 12)]]);
            page.change(Change::Write(Some(key(&page, text))));
            assert!(page.insert_picture(stored(1), cursor));
            assert_eq!(blocks(&page), expected, "{cursor:?}");
        }
    }

    #[test]
    fn pictures_move_by_paragraph_and_leave_the_text_whole() {
        let text = "One\n\nTwo\n\nThree";
        let mut page = editor(&[&[(text, 12)]]);
        let key = key(&page, text);
        page.change(Change::Write(Some(key)));
        page.insert_picture(stored(1), Some(5));
        for (change, expected) in [
            (PictureChange::Down, "One\n\nTwo|[1]|Three"),
            (PictureChange::Down, "One\n\nTwo\n\nThree|[1]"),
            (PictureChange::Down, "One\n\nTwo\n\nThree|[1]"),
            (PictureChange::Up, "One\n\nTwo|[1]|Three"),
            (PictureChange::Up, "One|[1]|Two\n\nThree"),
            (PictureChange::Up, "[1]|One\n\nTwo\n\nThree"),
            (PictureChange::Up, "[1]|One\n\nTwo\n\nThree"),
            (PictureChange::Remove, "One\n\nTwo\n\nThree"),
        ] {
            page.change(Change::Picture(key, 1, change));
            assert_eq!(blocks(&page), expected, "{change:?}");
        }
    }

    #[test]
    fn a_picture_alone_is_a_tile_and_can_be_written_under() {
        let mut page = Editor::new(0, &Layout::default());
        assert!(page.insert_picture(stored(1), None));
        assert_eq!((page.rows.len(), blocks(&page)), (1, "[1]".into()));
        let key = page.rows[0].tiles[0].key;
        page.change(Change::Write(Some(key)));
        *page.written().unwrap().1 = "Our guild".into();
        assert_eq!(blocks(&page), "[1]|Our guild");
    }

    #[test]
    fn a_page_takes_sixteen_pictures_with_its_banner() {
        let mut page = Editor::new(0, &Layout::default());
        page.set_banner(stored(99));
        for id in 1..MAX_PICTURES {
            assert!(page.insert_picture(stored(id as u8), None), "{id}");
        }
        assert!(!page.insert_picture(stored(50), None));
        assert!(!page.takes_picture());
        page.for_banner = true;
        assert!(page.takes_picture(), "another banner replaces this one");
    }

    #[test]
    fn a_new_page_starts_at_its_title() {
        let mut new = Editor::new(0, &Layout::default());
        assert_eq!(new.focus, Some(Focus::Title));
        assert!(new.writing.is_none() && new.rows.len() == 1 && new.layout().is_empty());
        new.title = "  Rules\nand dates ".into();
        assert_eq!(new.layout().title, "Rules and dates");
        new.title = "x".repeat(MAX_TITLE + 5);
        assert_eq!(new.layout().title.len(), MAX_TITLE);
    }

    #[test]
    fn adds_beside_and_removes_empty_rows() {
        let mut page = editor(&[&[("a", 12)]]);
        page.change(Change::Add(Place::Into(0, 1)));
        let (new, text) = page.written().unwrap();
        *text = "b  \n".into();
        assert_eq!(shown(&page), "a6+b6", "shared, and trailing space trimmed");
        page.change(Change::Add(Place::Into(0, 2)));
        *page.written().unwrap().1 = "c".into();
        assert_eq!(shown(&page), "a4+b4+c4");
        page.change(Change::Add(Place::Into(0, 3)));
        assert_eq!(page.rows[0].tiles.len(), 3, "a full row takes no more");
        for name in ["a", "c"] {
            page.change(Change::Remove(key(&page, name)));
        }
        page.change(Change::Remove(new.key));
        assert!(page.rows.is_empty() && page.writing.is_none());
    }

    #[test]
    fn dragging_an_edge_resizes() {
        let cases: [(Rows, &str, usize, &str); 9] = [
            (&[&[("a", 6)]], "a", 10, "a10"),
            (&[&[("a", 6)]], "a", 30, "a12"),
            (&[&[("a", 12)]], "a", 1, "a3"),
            (&[&[("a", 6), ("b", 6)]], "a", 8, "a8+b4"),
            (&[&[("a", 6), ("b", 6)]], "a", 11, "a9+b3"),
            (&[&[("a", 6), ("b", 6)]], "a", 4, "a4+b8"),
            (&[&[("a", 6), ("b", 6)]], "b", 4, "a6+b4"),
            (&[&[("a", 4), ("b", 4), ("c", 4)]], "a", 9, "a6+b3+c3"),
            (&[&[("a", 4), ("b", 4)]], "a", 7, "a7+b4"),
        ];
        for (rows, name, width, expected) in cases {
            let mut page = editor(rows);
            page.change(Change::Edge(key(&page, name), width));
            assert_eq!(shown(&page), expected, "{rows:?} {name} {width}");
        }
    }

    #[test]
    fn the_menu_resizes_any_tile() {
        let cases: [(Rows, &str, usize, &str); 3] = [
            (&[&[("a", 6), ("b", 6)]], "b", 8, "a4+b8"),
            (&[&[("a", 4), ("b", 5), ("c", 3)]], "c", 5, "a4+b3+c5"),
            (&[&[("a", 6), ("b", 6)]], "a", 4, "a4+b6"),
        ];
        for (rows, name, width, expected) in cases {
            let mut page = editor(rows);
            page.change(Change::Width(key(&page, name), width));
            assert_eq!(shown(&page), expected, "{rows:?} {name} {width}");
        }
    }

    #[test]
    fn drops_land_where_they_point() {
        let cases: [(Rows, &str, Place, &str); 8] = [
            (
                &[&[("a", 3), ("b", 5), ("c", 4)]],
                "a",
                Place::Into(0, 3),
                "b5+c4+a3",
            ),
            (&[&[("a", 3), ("b", 9)]], "b", Place::Into(0, 0), "b9+a3"),
            (
                &[&[("a", 12)], &[("b", 12)]],
                "a",
                Place::Into(1, 1),
                "b6+a6",
            ),
            (
                &[&[("a", 12)], &[("b", 7)]],
                "a",
                Place::Into(1, 0),
                "a5+b7",
            ),
            (&[&[("a", 4)], &[("b", 4)]], "a", Place::Into(1, 1), "b4+a4"),
            (
                &[&[("a", 7)], &[("b", 12)]],
                "a",
                Place::NewRow(2),
                "b12 a7",
            ),
            (&[&[("a", 6), ("b", 6)]], "b", Place::NewRow(0), "b12 a6"),
            (&[&[("a", 5)]], "a", Place::NewRow(1), "a5"),
        ];
        for (rows, name, place, expected) in cases {
            let mut page = editor(rows);
            page.change(Change::Move(key(&page, name), place));
            assert_eq!(shown(&page), expected, "{rows:?} {name} {place:?}");
        }
        let mut page = editor(&[&[("a", 8), ("b", 4)], &[("c", 12)]]);
        page.change(Change::Move(key(&page, "c"), Place::Swap(key(&page, "b"))));
        assert_eq!(shown(&page), "a8+c4 b12");
    }

    #[test]
    fn menu_steps_reach_every_neighbour() {
        let cases: [(Rows, &str, Step, &str); 7] = [
            (&[&[("a", 8), ("b", 4)]], "a", Step::Right, "b4+a8"),
            (&[&[("a", 8), ("b", 4)]], "b", Step::Left, "b4+a8"),
            (&[&[("a", 12)], &[("b", 5)]], "b", Step::Up, "b5 a12"),
            (&[&[("a", 12)], &[("b", 5)]], "a", Step::Down, "b5 a12"),
            (
                &[&[("a", 4), ("b", 4)], &[("c", 4)]],
                "c",
                Step::Up,
                "c4+a4+b4",
            ),
            (&[&[("a", 6), ("b", 6)]], "b", Step::Up, "b12 a6"),
            (&[&[("a", 6), ("b", 6)]], "a", Step::Down, "b6 a12"),
        ];
        for (rows, name, step, expected) in cases {
            let mut page = editor(rows);
            let key = key(&page, name);
            page.change(Change::Move(key, page.step(key, step).unwrap()));
            assert_eq!(shown(&page), expected, "{rows:?} {name} {step:?}");
        }
        let page = editor(&[&[("a", 12)]]);
        for step in [Step::Up, Step::Down, Step::Left, Step::Right] {
            assert_eq!(page.step(key(&page, "a"), step), None);
        }
    }

    #[test]
    fn any_run_of_changes_keeps_a_page_that_can_be_saved() {
        let mut page = editor(&[&[("a", 4), ("b", 4), ("c", 4)], &[("d", 7)]]);
        let mut seed = 7_usize;
        let mut pick = |below: usize| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 8) % below
        };
        for _ in 0..20_000 {
            let keys: Vec<Key> = (page.rows.iter())
                .flat_map(|row| row.tiles.iter().map(|tile| tile.key))
                .collect();
            let row = pick(page.rows.len() + 1);
            let place = match pick(3) {
                0 => Place::NewRow(row),
                1 => Place::Into(row, pick(4)),
                _ => Place::Swap(keys.get(pick(keys.len() + 1)).copied().unwrap_or(0)),
            };
            let key = keys.get(pick(keys.len() + 1)).copied().unwrap_or(0);
            let change = match pick(10) {
                0 | 1 => Change::Add(place),
                2 => Change::Remove(key),
                3 | 4 => Change::Edge(key, pick(16)),
                5 => Change::Width(key, pick(16)),
                _ => Change::Move(key, place),
            };
            page.change(change);
            let count: usize = page.rows.iter().map(|row| row.tiles.len()).sum();
            assert!(count <= MAX_TILES);
            for row in &page.rows {
                assert!((1..=MAX_COLUMNS).contains(&row.tiles.len()));
                assert!(
                    row.tiles
                        .iter()
                        .all(|t| (MIN_WIDTH..=GRID).contains(&t.width))
                );
                assert!(row.tiles.iter().map(|t| t.width).sum::<usize>() <= GRID);
            }
            if !matches!(change, Change::Add(_) | Change::Remove(_)) {
                assert_eq!(count, keys.len(), "a change lost a tile");
            }
        }
    }
}
