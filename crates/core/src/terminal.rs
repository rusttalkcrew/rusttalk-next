//! A terminal shared in a call: the messages between its sharer and its viewers, and
//! what each side keeps track of.
//!
//! The shell runs only on the sharer's computer. Viewers replay its output in their own
//! terminal emulator, at the sharer's size. Output is numbered, so a viewer that misses
//! some asks for the current screen again. Messages travel sealed (see `client::voice`),
//! so the media server can neither read nor forge them.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

/// Random handle for one share, so messages from an earlier one are ignored.
pub type Id = [u8; 16];

/// Output bytes per message. Sealed and base64-encoded, this stays well under LiveKit's
/// data packet limit.
pub const MAX_CHUNK: usize = 8 * 1024;
/// Terminal sizes either side accepts.
pub const MAX_COLS: u16 = 500;
pub const MAX_ROWS: u16 = 200;
/// The biggest screen a viewer puts together from snapshot parts.
pub const MAX_SNAPSHOT: usize = 2 * 1024 * 1024;
/// Each viewer gets at most one fresh screen this often, so asking can't flood the sharer.
pub const SNAPSHOT_EVERY: Duration = Duration::from_millis(500);
/// What a viewer takes from one share: this many bytes at once, refilled at `VIEW_RATE`
/// per second. More asks for a fresh screen instead, so a sharer can't flood a viewer.
const VIEW_BURST: f64 = 4.0 * 1024.0 * 1024.0;
const VIEW_RATE: f64 = 2.0 * 1024.0 * 1024.0;

/// What a sharer announces to the call (as a sealed participant attribute) while sharing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Share {
    pub id: Id,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    /// Viewer to sharer: send me the screen, and its output from then on. Also sent
    /// again after missing output.
    Watch { id: Id },
    /// Viewer to sharer: stop sending.
    Unwatch { id: Id },
    /// Sharer to one viewer: the screen as of output `seq`, as bytes that redraw it in a
    /// fresh terminal of this size. Split over several messages; the last has `more` false.
    Snapshot {
        id: Id,
        seq: u64,
        cols: u16,
        rows: u16,
        #[serde(with = "bytes")]
        data: Vec<u8>,
        more: bool,
    },
    /// Sharer to viewers: output that follows `seq - 1`.
    Data {
        id: Id,
        seq: u64,
        #[serde(with = "bytes")]
        data: Vec<u8>,
    },
    /// Sharer to viewers: the terminal took this size, in order with the output.
    Resize {
        id: Id,
        seq: u64,
        cols: u16,
        rows: u16,
    },
    /// Sharer to viewers: the share stopped.
    End { id: Id },
}

fn size_ok(cols: u16, rows: u16) -> bool {
    (2..=MAX_COLS).contains(&cols) && (1..=MAX_ROWS).contains(&rows)
}

impl Message {
    pub fn id(&self) -> Id {
        match self {
            Self::Watch { id }
            | Self::Unwatch { id }
            | Self::Snapshot { id, .. }
            | Self::Data { id, .. }
            | Self::Resize { id, .. }
            | Self::End { id } => *id,
        }
    }

    pub fn valid(&self) -> bool {
        match self {
            Self::Snapshot {
                cols, rows, data, ..
            } => size_ok(*cols, *rows) && data.len() <= MAX_CHUNK,
            Self::Data { data, .. } => data.len() <= MAX_CHUNK,
            Self::Resize { cols, rows, .. } => size_ok(*cols, *rows),
            Self::Watch { .. } | Self::Unwatch { .. } | Self::End { .. } => true,
        }
    }
}

/// Output split into messages that each fit, numbered by `host`.
pub fn data(host: &mut Host, bytes: &[u8]) -> Vec<Message> {
    bytes
        .chunks(MAX_CHUNK)
        .map(|chunk| Message::Data {
            id: host.id,
            seq: host.next_seq(),
            data: chunk.to_vec(),
        })
        .collect()
}

/// The terminal took a new size, numbered by `host` in order with the output.
pub fn resize(host: &mut Host, cols: u16, rows: u16) -> Message {
    Message::Resize {
        id: host.id,
        seq: host.next_seq(),
        cols,
        rows,
    }
}

/// A screen as of `host`'s last output, split into messages that each fit.
pub fn snapshot(host: &Host, cols: u16, rows: u16, bytes: &[u8]) -> Vec<Message> {
    let mut chunks: Vec<&[u8]> = bytes.chunks(MAX_CHUNK).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let last = chunks.len() - 1;
    chunks
        .into_iter()
        .enumerate()
        .map(|(at, chunk)| Message::Snapshot {
            id: host.id,
            seq: host.seq,
            cols,
            rows,
            data: chunk.to_vec(),
            more: at < last,
        })
        .collect()
}

/// The sharer's side: who watches, and the numbering of the output.
pub struct Host {
    pub id: Id,
    seq: u64,
    watchers: BTreeSet<String>,
    /// When each device last got a screen. Kept after they stop watching, until they
    /// leave or the share ends, so watching again doesn't skip the wait.
    screens: BTreeMap<String, Instant>,
    /// Watchers waiting for a screen.
    owed: BTreeSet<String>,
}

impl Default for Host {
    fn default() -> Self {
        let mut id = [0; 16];
        crate::identity::random_bytes(&mut id).expect("system randomness");
        Self {
            id,
            seq: 0,
            watchers: BTreeSet::new(),
            screens: BTreeMap::new(),
            owed: BTreeSet::new(),
        }
    }
}

impl Host {
    pub fn watchers(&self) -> impl Iterator<Item = &str> {
        self.watchers.iter().map(String::as_str)
    }

    /// The number of the output sent last.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// A viewer's message. Anyone in the call may watch.
    pub fn receive(&mut self, from: &str, message: &Message) {
        match message {
            Message::Watch { id } if *id == self.id => {
                self.watchers.insert(from.to_owned());
                self.owed.insert(from.to_owned());
            }
            Message::Unwatch { id } if *id == self.id => {
                self.watchers.remove(from);
                self.owed.remove(from);
            }
            _ => {}
        }
    }

    /// Someone left the call.
    pub fn left(&mut self, who: &str) {
        self.watchers.remove(who);
        self.owed.remove(who);
        self.screens.remove(who);
    }

    /// This watcher needs the screen again.
    pub fn owe(&mut self, who: &str) {
        if self.watchers.contains(who) {
            self.owed.insert(who.to_owned());
        }
    }

    /// Every watcher needs the screen again: output was dropped to keep up.
    pub fn resync(&mut self) {
        self.owed.extend(self.watchers.iter().cloned());
    }

    /// Watchers who get a screen now; the others wait until `SNAPSHOT_EVERY` has passed.
    pub fn due(&mut self, now: Instant) -> Vec<String> {
        let mut due = Vec::new();
        for who in &self.watchers {
            let last = self.screens.get(who);
            let ready = last.is_none_or(|last| now.duration_since(*last) >= SNAPSHOT_EVERY);
            if ready && self.owed.remove(who) {
                self.screens.insert(who.clone(), now);
                due.push(who.clone());
            }
        }
        due
    }
}

/// What a viewer does with a message.
#[derive(Debug, PartialEq)]
pub enum Feed {
    Nothing,
    /// Start over with a fresh terminal of this size, then play these bytes.
    Reset {
        cols: u16,
        rows: u16,
        data: Vec<u8>,
    },
    Bytes(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Output went missing: ask for the screen again (send `Viewer::watch`).
    Resync,
    /// The share stopped.
    Ended,
}

enum Sync {
    /// Waiting for a screen.
    Waiting,
    Assembling {
        seq: u64,
        cols: u16,
        rows: u16,
        data: Vec<u8>,
    },
    /// Up to date with output `seq`.
    Live(u64),
}

/// The viewer's side of one shared terminal.
pub struct Viewer {
    pub sharer: String,
    pub id: Id,
    sync: Sync,
    /// Bytes we may still take now, and when that was worked out.
    budget: f64,
    at: Instant,
}

impl Viewer {
    pub fn new(sharer: &str, id: Id) -> Self {
        Self {
            sharer: sharer.to_owned(),
            id,
            sync: Sync::Waiting,
            budget: VIEW_BURST,
            at: Instant::now(),
        }
    }

    /// The message that asks for the screen.
    pub fn watch(&self) -> Message {
        Message::Watch { id: self.id }
    }

    /// Handle a message `from` someone; only this share's sharer counts.
    pub fn receive(&mut self, from: &str, message: Message) -> Feed {
        self.receive_at(from, message, Instant::now())
    }

    fn receive_at(&mut self, from: &str, message: Message, now: Instant) -> Feed {
        if from != self.sharer || message.id() != self.id {
            return Feed::Nothing;
        }
        let size = match &message {
            Message::Snapshot { data, .. } | Message::Data { data, .. } => data.len() as f64,
            _ => 0.0,
        };
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.budget = (self.budget + elapsed * VIEW_RATE).min(VIEW_BURST);
        self.at = now;
        if size > self.budget {
            // Too much too fast: drop it, and take a fresh screen when there's room.
            let resync =
                matches!(message, Message::Snapshot { .. }) || !matches!(self.sync, Sync::Waiting);
            self.sync = Sync::Waiting;
            return if resync { Feed::Resync } else { Feed::Nothing };
        }
        self.budget -= size;
        match message {
            Message::Snapshot {
                seq,
                cols,
                rows,
                data,
                more,
                ..
            } => {
                let mut buffer = match std::mem::replace(&mut self.sync, Sync::Waiting) {
                    Sync::Assembling {
                        seq: s,
                        cols: c,
                        rows: r,
                        data,
                    } if (s, c, r) == (seq, cols, rows) => data,
                    _ => Vec::new(),
                };
                buffer.extend_from_slice(&data);
                if buffer.len() > MAX_SNAPSHOT {
                    return Feed::Resync;
                }
                if more {
                    self.sync = Sync::Assembling {
                        seq,
                        cols,
                        rows,
                        data: buffer,
                    };
                    return Feed::Nothing;
                }
                self.sync = Sync::Live(seq);
                Feed::Reset {
                    cols,
                    rows,
                    data: buffer,
                }
            }
            Message::Data { seq, data, .. } => self.next(seq, Feed::Bytes(data)),
            Message::Resize {
                seq, cols, rows, ..
            } => self.next(seq, Feed::Resize { cols, rows }),
            Message::End { .. } => Feed::Ended,
            Message::Watch { .. } | Message::Unwatch { .. } => Feed::Nothing,
        }
    }

    /// Play output that comes right after what we have; ask again after a gap.
    fn next(&mut self, seq: u64, feed: Feed) -> Feed {
        match self.sync {
            Sync::Live(last) if seq == last + 1 => {
                self.sync = Sync::Live(seq);
                feed
            }
            // Already in the screen we got.
            Sync::Live(last) if seq <= last => Feed::Nothing,
            Sync::Live(_) => {
                self.sync = Sync::Waiting;
                Feed::Resync
            }
            // Output for the screen we're waiting for is in it.
            Sync::Waiting | Sync::Assembling { .. } => Feed::Nothing,
        }
    }
}

/// Bytes as base64 in JSON, rather than an array of numbers.
mod bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = <&str>::deserialize(deserializer)?;
        STANDARD.decode(text).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHARER: &str = "sharer";

    #[test]
    fn messages_round_trip_and_validate() {
        let messages = [
            Message::Watch { id: [1; 16] },
            Message::Snapshot {
                id: [1; 16],
                seq: 4,
                cols: 80,
                rows: 24,
                data: b"\x1b[1mhi".to_vec(),
                more: true,
            },
            Message::Data {
                id: [1; 16],
                seq: 5,
                data: vec![0, 255, 10],
            },
            Message::Resize {
                id: [1; 16],
                seq: 6,
                cols: 100,
                rows: 30,
            },
            Message::End { id: [1; 16] },
        ];
        for message in messages {
            let wire = serde_json::to_vec(&message).unwrap();
            let back: Message = serde_json::from_slice(&wire).unwrap();
            assert_eq!(back, message);
            assert!(back.valid());
        }
        let data = serde_json::to_string(&Message::Data {
            id: [0; 16],
            seq: 1,
            data: b"hello".to_vec(),
        })
        .unwrap();
        assert!(data.contains("\"aGVsbG8=\""), "{data}");
        let big = Message::Data {
            id: [0; 16],
            seq: 1,
            data: vec![0; MAX_CHUNK + 1],
        };
        assert!(!big.valid());
        for (cols, rows) in [(1, 24), (80, 0), (MAX_COLS + 1, 24), (80, MAX_ROWS + 1)] {
            let resize = Message::Resize {
                id: [0; 16],
                seq: 1,
                cols,
                rows,
            };
            assert!(!resize.valid());
        }
    }

    #[test]
    fn a_full_message_stays_under_the_data_packet_limit() {
        let message = Message::Snapshot {
            id: [255; 16],
            seq: u64::MAX,
            cols: MAX_COLS,
            rows: MAX_ROWS,
            data: vec![255; MAX_CHUNK],
            more: true,
        };
        assert!(serde_json::to_vec(&message).unwrap().len() < 12 * 1024);
    }

    /// The sharer has a watcher who got their screen.
    fn watched(now: Instant) -> (Host, Viewer) {
        let mut host = Host::default();
        let mut viewer = Viewer::new(SHARER, host.id);
        host.receive("ada", &viewer.watch());
        assert_eq!(host.due(now), ["ada"]);
        let parts = snapshot(&host, 80, 24, &vec![b'x'; MAX_CHUNK + 10]);
        assert_eq!(parts.len(), 2);
        let mut feeds: Vec<_> = parts
            .into_iter()
            .map(|part| viewer.receive(SHARER, part))
            .collect();
        let Some(Feed::Reset { cols, rows, data }) = feeds.pop() else {
            panic!("no screen");
        };
        assert_eq!((cols, rows, data.len()), (80, 24, MAX_CHUNK + 10));
        assert_eq!(feeds, [Feed::Nothing]);
        (host, viewer)
    }

    #[test]
    fn output_follows_the_screen_in_order() {
        let now = Instant::now();
        let (mut host, mut viewer) = watched(now);
        let first = data(&mut host, b"one");
        let second = data(&mut host, b"two");
        assert_eq!(
            viewer.receive(SHARER, first[0].clone()),
            Feed::Bytes(b"one".to_vec())
        );
        // A repeat is ignored.
        assert_eq!(viewer.receive(SHARER, first[0].clone()), Feed::Nothing);
        assert_eq!(
            viewer.receive(SHARER, second[0].clone()),
            Feed::Bytes(b"two".to_vec())
        );
        // Big output is split and numbered in order.
        let big = data(&mut host, &vec![0; 2 * MAX_CHUNK + 1]);
        assert_eq!(big.len(), 3);
        for part in big {
            assert!(matches!(viewer.receive(SHARER, part), Feed::Bytes(_)));
        }
        assert_eq!(
            viewer.receive(SHARER, Message::End { id: host.id }),
            Feed::Ended
        );
    }

    #[test]
    fn a_gap_asks_for_the_screen_again() {
        let now = Instant::now();
        let (mut host, mut viewer) = watched(now);
        let _lost = data(&mut host, b"lost");
        let later = data(&mut host, b"later");
        assert_eq!(viewer.receive(SHARER, later[0].clone()), Feed::Resync);
        // Until the new screen arrives, output is part of it.
        let more = data(&mut host, b"more");
        assert_eq!(viewer.receive(SHARER, more[0].clone()), Feed::Nothing);
        host.receive("ada", &viewer.watch());
        // Not before the last screen is old enough.
        assert!(host.due(now).is_empty());
        let later_on = now + SNAPSHOT_EVERY;
        assert_eq!(host.due(later_on), ["ada"]);
        let screen = snapshot(&host, 80, 24, b"screen");
        assert!(matches!(
            viewer.receive(SHARER, screen[0].clone()),
            Feed::Reset { .. }
        ));
        let next = data(&mut host, b"next");
        assert_eq!(
            viewer.receive(SHARER, next[0].clone()),
            Feed::Bytes(b"next".to_vec())
        );
    }

    #[test]
    fn only_the_sharer_and_this_share_count() {
        let now = Instant::now();
        let (mut host, mut viewer) = watched(now);
        let next = data(&mut host, b"next");
        assert_eq!(viewer.receive("mallory", next[0].clone()), Feed::Nothing);
        let other = Message::End { id: [9; 16] };
        assert_eq!(viewer.receive(SHARER, other), Feed::Nothing);
        // A watch for another share adds nobody.
        host.receive("bob", &Message::Watch { id: [9; 16] });
        assert_eq!(host.watchers().collect::<Vec<_>>(), ["ada"]);
        host.receive("ada", &Message::Unwatch { id: host.id });
        assert_eq!(host.watchers().count(), 0);
    }

    #[test]
    fn resync_sends_everyone_a_screen_in_turn() {
        let now = Instant::now();
        let (mut host, _) = watched(now);
        host.receive("bob", &Message::Watch { id: host.id });
        host.resync();
        // Bob never had a screen; Ada waits until hers is old enough.
        assert_eq!(host.due(now), ["bob"]);
        assert_eq!(host.due(now + SNAPSHOT_EVERY), ["ada"]);
        assert!(host.due(now + 4 * SNAPSHOT_EVERY).is_empty());
        host.left("ada");
        host.resync();
        assert_eq!(host.due(now + 8 * SNAPSHOT_EVERY), ["bob"]);
    }

    #[test]
    fn an_oversized_screen_is_dropped() {
        let mut viewer = Viewer::new(SHARER, [1; 16]);
        let part = |more| Message::Snapshot {
            id: [1; 16],
            seq: 1,
            cols: 80,
            rows: 24,
            data: vec![0; MAX_CHUNK],
            more,
        };
        let mut last = Feed::Nothing;
        for _ in 0..=MAX_SNAPSHOT / MAX_CHUNK {
            last = viewer.receive(SHARER, part(true));
        }
        assert_eq!(last, Feed::Resync);
    }

    #[test]
    fn watching_again_doesnt_skip_the_wait_for_a_screen() {
        let now = Instant::now();
        let (mut host, viewer) = watched(now);
        for _ in 0..5 {
            host.receive("ada", &Message::Unwatch { id: host.id });
            host.receive("ada", &viewer.watch());
            assert!(host.due(now).is_empty());
        }
        assert_eq!(host.due(now + SNAPSHOT_EVERY), ["ada"]);
        // Leaving the call forgets it.
        host.left("ada");
        host.receive("ada", &viewer.watch());
        assert_eq!(host.due(now + SNAPSHOT_EVERY), ["ada"]);
    }

    #[test]
    fn a_flood_of_output_asks_for_a_screen_instead() {
        let now = Instant::now();
        let (mut host, mut viewer) = watched(now);
        let flood = data(&mut host, &vec![0; VIEW_BURST as usize + MAX_CHUNK]);
        let feeds: Vec<_> = (flood.into_iter())
            .map(|message| viewer.receive_at(SHARER, message, now))
            .collect();
        assert_eq!(feeds.iter().filter(|f| **f == Feed::Resync).count(), 1);
        // Later there's room again for a screen and what follows it.
        let later = now + Duration::from_secs(2);
        let screen = snapshot(&host, 80, 24, b"screen");
        let reset = viewer.receive_at(SHARER, screen[0].clone(), later);
        assert!(matches!(reset, Feed::Reset { .. }));
        let next = data(&mut host, b"next");
        let bytes = viewer.receive_at(SHARER, next[0].clone(), later);
        assert_eq!(bytes, Feed::Bytes(b"next".to_vec()));
    }
}
