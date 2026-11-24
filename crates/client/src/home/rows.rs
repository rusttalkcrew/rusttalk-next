//! A long chat lays out only the messages near the view (#461). The rest take the
//! height they had when last drawn (or a typical one, before that). So nothing
//! jumps when heights become known or rows arrive, the scroll offset is planned
//! before the frame: following the newest message, the end of the list stays at
//! the bottom; scrolled up, the first message in view stays where it was.

use std::collections::{HashMap, HashSet};

use crate::server::Id;

/// A message row: its author, its ID, and whether it starts a group (with avatar
/// and name), which changes its height the most.
pub type Key = (Id, Id, bool);

/// Height for a row never drawn, until some are measured.
const TYPICAL: f32 = 40.0;

/// A chat's measured row heights and where its view was last frame.
#[derive(Default)]
pub struct Rows {
    heights: HashMap<Key, f32>,
    /// What came above the first message last frame: the chat's start or a spinner.
    pub(crate) header: f32,
    /// What came below the last message (messages being sent), and the view's height.
    tail: f32,
    view: f32,
    /// The first row in view and how far the view's top was below that row's top.
    anchor: Option<(Key, f32)>,
    /// The view was scrolled up from the newest message.
    scrolled_up: bool,
    /// How many rows were laid out in the last frame.
    pub drawn: usize,
}

/// How a frame of the list came out.
pub struct Frame<'a> {
    pub keys: &'a [Key],
    /// Each row's top, and where the rows ended.
    pub tops: &'a [f32],
    pub end: f32,
    pub content: f32,
    pub view: f32,
    /// The scroll offset at the end of the frame, and how far it moved during it.
    pub offset: f32,
    pub moved: f32,
    /// A row's height was not what was assumed: the frame is laid out again.
    pub changed: bool,
}

impl Rows {
    /// Each row's height as far as known, and the scroll offset that keeps the
    /// view in place (`None` when it needs no change).
    pub fn plan(&self, keys: &[Key], offset: f32) -> (Vec<f32>, Option<f32>) {
        let known: Vec<Option<f32>> = (keys.iter())
            .map(|key| self.heights.get(key).copied())
            .collect();
        let (sum, count) = (known.iter().flatten()).fold((0.0, 0), |(sum, n), h| (sum + h, n + 1));
        let typical = if count > 0 {
            (sum / count as f32).round()
        } else {
            TYPICAL
        };
        let heights: Vec<f32> = known.into_iter().map(|h| h.unwrap_or(typical)).collect();
        let keep = if self.scrolled_up {
            self.anchor.and_then(|(anchor, into)| {
                let at = keys.iter().position(|key| *key == anchor)?;
                Some(self.header + heights[..at].iter().sum::<f32>() + into)
            })
        } else if self.view > 0.0 {
            let content = self.header + heights.iter().sum::<f32>() + self.tail;
            Some((content - self.view).max(0.0))
        } else {
            // The first frame: nothing measured yet.
            None
        };
        (heights, keep.filter(|keep| (keep - offset).abs() > 0.5))
    }

    /// Remember a row as it was just drawn. Returns whether it differs from what
    /// was assumed for it.
    pub fn measured(&mut self, key: Key, assumed: f32, height: f32) -> bool {
        self.heights.insert(key, height);
        (assumed - height).abs() > 0.5
    }

    /// After the frame: remember the row at the top of the view, and whether the
    /// view follows the newest message. A frame that is laid out again doesn't
    /// count: the row in view stays, moved only as far as the view was scrolled.
    pub fn settle(&mut self, frame: Frame) {
        self.tail = frame.content - frame.end;
        self.view = frame.view;
        self.scrolled_up = frame.offset < frame.content - frame.view - 0.5;
        if frame.changed {
            if let Some((_, into)) = &mut self.anchor {
                *into += frame.moved;
            }
            return;
        }
        let first = (frame.tops)
            .partition_point(|top| *top <= frame.offset)
            .saturating_sub(1);
        self.anchor = (frame.keys.get(first)).map(|key| (*key, frame.offset - frame.tops[first]));
        // Forget rows that are gone, now and then.
        if self.heights.len() > 2 * frame.keys.len() + 256 {
            let keep: HashSet<&Key> = frame.keys.iter().collect();
            self.heights.retain(|key, _| keep.contains(key));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> Key {
        ([1; 32], [n; 32], n.is_multiple_of(3))
    }

    fn frame<'a>(keys: &'a [Key], tops: &'a [f32], offset: f32, moved: f32) -> Frame<'a> {
        Frame {
            keys,
            tops,
            end: tops.len() as f32 * 30.0,
            content: tops.len() as f32 * 30.0,
            view: 100.0,
            offset,
            moved,
            changed: moved != 0.0,
        }
    }

    /// Rows 10 to 19 of 30 px; then rows 0 to 9 arrive above them.
    #[test]
    fn rows_added_above_keep_the_row_in_view_in_place() {
        let mut rows = Rows::default();
        let keys: Vec<Key> = (10..20).map(key).collect();
        for k in &keys {
            rows.measured(*k, TYPICAL, 30.0);
        }
        let tops: Vec<f32> = (0..10).map(|i| i as f32 * 30.0).collect();
        // The view starts 5 px into row 14.
        rows.settle(frame(&keys, &tops, 125.0, 0.0));
        let (heights, keep) = rows.plan(&keys, 125.0);
        assert_eq!(heights, [30.0; 10]);
        assert_eq!(keep, None);
        let more: Vec<Key> = (0..20).map(key).collect();
        let (heights, keep) = rows.plan(&more, 125.0);
        // Rows never drawn take the typical height of those that were.
        assert_eq!(heights, [30.0; 20]);
        assert_eq!(keep, Some(425.0));
        // Following the newest message, the end stays at the bottom instead.
        rows.settle(frame(&keys, &tops, 200.0, 0.0));
        assert_eq!(rows.plan(&keys, 200.0).1, None);
        assert_eq!(rows.plan(&more, 200.0).1, Some(500.0));
    }

    /// A frame laid out again keeps its row in view, moved only by scrolling.
    #[test]
    fn a_frame_laid_out_again_keeps_its_row_in_view() {
        let mut rows = Rows::default();
        let keys: Vec<Key> = (0..10).map(key).collect();
        let tops: Vec<f32> = (0..10).map(|i| i as f32 * 30.0).collect();
        rows.settle(frame(&keys, &tops, 65.0, 0.0));
        assert_eq!(rows.anchor, Some((key(2), 5.0)));
        rows.settle(frame(&keys, &tops, 150.0, 12.0));
        assert_eq!(rows.anchor, Some((key(2), 17.0)));
    }
}
