//! Pokes (#112): a short note that pops up for someone, TeamSpeak style.
//!
//! A poke is a DM message with the poke flag, so it is sealed like any DM and goes
//! over every server both people share. The history of received and sent pokes
//! stays on this device only (`pokes.json`).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::server::Id;

/// The most pokes kept; the oldest go first.
const KEEP: usize = 200;
/// Pokes taken from one person per `WINDOW` seconds; more are dropped unseen.
const LIMIT: usize = 3;
const WINDOW: i64 = 60;
/// A poke loaded from history rather than live counts as unread this long after it
/// was sent, so pokes sent while the app was closed still show a badge.
const FRESH: i64 = 24 * 60 * 60;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Poke {
    /// The message ID, the same on every server holding a copy.
    pub id: Id,
    /// The other person: who sent it, or who it went to.
    pub person: Id,
    pub incoming: bool,
    pub text: String,
    /// Unix seconds.
    pub at: i64,
    /// The server's name when it came, for the list.
    pub server: String,
    pub read: bool,
}

#[derive(Default)]
pub struct Pokes {
    list: Vec<Poke>,
    path: Option<PathBuf>,
    /// Server times of the pokes taken from each person lately, for `LIMIT`.
    recent: HashMap<Id, VecDeque<i64>>,
}

impl Pokes {
    pub fn open(path: PathBuf) -> Self {
        let list = crate::load_json(&path).unwrap_or_else(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "poke history unreadable");
            Vec::new()
        });
        Self {
            list,
            path: Some(path),
            recent: HashMap::new(),
        }
    }

    /// Newest first.
    pub fn list(&self) -> impl Iterator<Item = &Poke> {
        self.list.iter().rev()
    }

    pub fn unread(&self) -> usize {
        self.list.iter().filter(|poke| !poke.read).count()
    }

    pub fn mark_read(&mut self) {
        if self.unread() > 0 {
            self.list.iter_mut().for_each(|poke| poke.read = true);
            self.save();
        }
    }

    /// Keep a poke, unless it is known already (a copy on another server or a reload)
    /// or its sender went over the limit. `live` ones are unread; loaded ones only
    /// while fresh. Returns whether it was new and kept.
    pub fn add(&mut self, mut poke: Poke, live: bool, now: i64) -> bool {
        let known = (self.list.iter()).any(|seen| seen.id == poke.id && seen.person == poke.person);
        if known {
            return false;
        }
        if poke.incoming {
            let recent = self.recent.entry(poke.person).or_default();
            recent.retain(|at| (poke.at - WINDOW..=poke.at).contains(at));
            if recent.len() >= LIMIT {
                tracing::debug!("poke dropped: too many from one person");
                return false;
            }
            recent.push_back(poke.at);
        }
        poke.read = !poke.incoming || !(live || poke.at > now - FRESH);
        // Kept in the order they come; loaded ones may be older than the newest.
        let at = self.list.partition_point(|seen| seen.at <= poke.at);
        self.list.insert(at, poke);
        if self.list.len() > KEEP {
            self.list.remove(0);
        }
        self.save();
        true
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let dir = path.parent().expect("in the data folder");
        if let Err(error) = rusttalk_core::client::save_json(dir, path, &self.list) {
            tracing::warn!(error = %format_args!("{error:#}"), "saving the poke history failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poke(id: u8, person: u8, at: i64) -> Poke {
        Poke {
            id: [id; 32],
            person: [person; 32],
            incoming: true,
            text: "hi".into(),
            at,
            server: "s".into(),
            read: false,
        }
    }

    #[test]
    fn copies_count_once_and_each_sender_is_limited() {
        let mut pokes = Pokes::default();
        assert!(pokes.add(poke(1, 1, 100), true, 100));
        // The copy from another server, or the same one loaded again.
        assert!(!pokes.add(poke(1, 1, 101), false, 101));
        assert!(pokes.add(poke(2, 1, 110), true, 110));
        assert!(pokes.add(poke(3, 1, 120), true, 120));
        // A fourth within a minute is dropped; someone else still gets through.
        assert!(!pokes.add(poke(4, 1, 130), true, 130));
        assert!(pokes.add(poke(5, 2, 130), true, 130));
        // A minute after the first, one more fits.
        assert!(pokes.add(poke(6, 1, 161), true, 161));
        assert_eq!(pokes.unread(), 5);
        pokes.mark_read();
        assert_eq!(pokes.unread(), 0);
        let newest: Vec<u8> = pokes.list().map(|poke| poke.id[0]).collect();
        assert_eq!(newest, [6, 5, 3, 2, 1]);
    }

    #[test]
    fn old_pokes_loaded_from_history_are_read() {
        let mut pokes = Pokes::default();
        let now = 10 * FRESH;
        assert!(pokes.add(poke(1, 1, now - FRESH - 1), false, now));
        assert!(pokes.add(poke(2, 2, now - 60), false, now));
        assert_eq!(pokes.unread(), 1);
        let sent = Poke {
            incoming: false,
            ..poke(3, 1, now)
        };
        assert!(pokes.add(sent, true, now));
        assert_eq!(pokes.unread(), 1);
    }
}
