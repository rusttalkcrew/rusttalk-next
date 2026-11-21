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
/// Typed bytes per message; a longer paste is split.
pub const MAX_INPUT: usize = 1024;
/// What someone may type: this many bytes at once, refilled at `TYPE_RATE` per second.
const TYPE_BURST: f64 = 16.0 * 1024.0;
const TYPE_RATE: f64 = 8.0 * 1024.0;
/// A request nobody answered in this time counts as denied.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Random, unguessable handle for the right to type, given with a grant.
pub type Session = [u8; 16];

fn random() -> [u8; 16] {
    let mut bytes = [0; 16];
    crate::identity::random_bytes(&mut bytes).expect("system randomness");
    bytes
}

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
    /// Viewer to sharer: may I type?
    Request { id: Id },
    /// Viewer to sharer: never mind, or stop typing.
    Withdraw { id: Id },
    /// Sharer to viewer: yes; send what you type with `session`.
    Grant { id: Id, session: Session },
    /// Sharer to viewer: no, or the request lapsed.
    Deny { id: Id },
    /// Sharer to viewer: typing taken back.
    Revoke { id: Id },
    /// Viewer to sharer: typed bytes. `seq` rises with every message, so a replay is dropped.
    Input {
        id: Id,
        session: Session,
        seq: u64,
        #[serde(with = "bytes")]
        data: Vec<u8>,
    },
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
            | Self::End { id }
            | Self::Request { id }
            | Self::Withdraw { id }
            | Self::Grant { id, .. }
            | Self::Deny { id }
            | Self::Revoke { id }
            | Self::Input { id, .. } => *id,
        }
    }

    pub fn valid(&self) -> bool {
        match self {
            Self::Snapshot {
                cols, rows, data, ..
            } => size_ok(*cols, *rows) && data.len() <= MAX_CHUNK,
            Self::Data { data, .. } => data.len() <= MAX_CHUNK,
            Self::Resize { cols, rows, .. } => size_ok(*cols, *rows),
            Self::Input { data, .. } => !data.is_empty() && data.len() <= MAX_INPUT,
            _ => true,
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

/// Someone the sharer let type.
struct Typist {
    session: Session,
    /// The last `seq` taken, so a replay is dropped.
    seq: u64,
    /// Bytes they may still type now, and when that was worked out.
    budget: f64,
    at: Instant,
}

/// What the sharer does with a viewer's message.
#[derive(Debug, PartialEq)]
pub enum Step {
    Nothing,
    /// Show the prompt: someone asks to type.
    Ask,
    /// Answer the sender with this.
    Reply(Message),
    /// Write these bytes to the shell.
    Type(Vec<u8>),
    /// An asker gave up, or someone stopped typing: update the prompt and the badges.
    Ended,
}

/// The sharer's side: who watches, the numbering of the output, and who may type.
pub struct Host {
    pub id: Id,
    seq: u64,
    watchers: BTreeSet<String>,
    /// When each device last got a screen. Kept after they stop watching, until they
    /// leave or the share ends, so watching again doesn't skip the wait.
    screens: BTreeMap<String, Instant>,
    /// Watchers waiting for a screen.
    owed: BTreeSet<String>,
    /// Who asks to type, since when.
    asking: BTreeMap<String, Instant>,
    typists: BTreeMap<String, Typist>,
}

impl Default for Host {
    fn default() -> Self {
        Self {
            id: random(),
            seq: 0,
            watchers: BTreeSet::new(),
            screens: BTreeMap::new(),
            owed: BTreeSet::new(),
            asking: BTreeMap::new(),
            typists: BTreeMap::new(),
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

    /// A viewer's message. Anyone in the call may watch; only watchers may ask to type,
    /// and only those the sharer allowed may. `online` says whether the sharer is
    /// connected to the server and so knows who the sender is: without that nobody may
    /// ask or type.
    pub fn receive(&mut self, from: &str, message: Message, online: bool, now: Instant) -> Step {
        if message.id() != self.id {
            return Step::Nothing;
        }
        let id = self.id;
        match message {
            Message::Watch { .. } => {
                self.watchers.insert(from.to_owned());
                self.owed.insert(from.to_owned());
                Step::Nothing
            }
            Message::Unwatch { .. } => self.unwatch(from),
            Message::Request { .. } => {
                if !online || !self.watchers.contains(from) {
                    return Step::Reply(Message::Deny { id });
                }
                if self.typists.contains_key(from) {
                    return Step::Nothing;
                }
                // Asking again restarts the wait.
                self.asking.insert(from.to_owned(), now);
                Step::Ask
            }
            Message::Withdraw { .. } => {
                let asked = self.asking.remove(from).is_some();
                let typed = self.typists.remove(from).is_some();
                if asked || typed {
                    Step::Ended
                } else {
                    Step::Nothing
                }
            }
            Message::Input {
                session, seq, data, ..
            } => {
                let Some(typist) = self.typists.get_mut(from) else {
                    return Step::Nothing;
                };
                if !online || session != typist.session || seq <= typist.seq {
                    return Step::Nothing;
                }
                typist.seq = seq;
                let elapsed = now.saturating_duration_since(typist.at).as_secs_f64();
                typist.budget = (typist.budget + elapsed * TYPE_RATE).min(TYPE_BURST);
                typist.at = now;
                if (data.len() as f64) > typist.budget {
                    return Step::Nothing;
                }
                typist.budget -= data.len() as f64;
                Step::Type(data)
            }
            _ => Step::Nothing,
        }
    }

    /// Who asks to type now, longest waiting first.
    pub fn askers(&self, now: Instant) -> Vec<&str> {
        let mut askers: Vec<_> = (self.asking.iter())
            .filter(|(_, at)| now.duration_since(**at) < REQUEST_TIMEOUT)
            .collect();
        askers.sort_by_key(|(_, at)| **at);
        askers.into_iter().map(|(who, _)| who.as_str()).collect()
    }

    /// Who may type.
    pub fn typists(&self) -> impl Iterator<Item = &str> {
        self.typists.keys().map(String::as_str)
    }

    /// The sharer answered `who`'s request. `None` unless that request is still open:
    /// an "Allow" never outlives the request it was meant for.
    pub fn answer(&mut self, who: &str, allow: bool, now: Instant) -> Option<Message> {
        let at = self.asking.remove(who)?;
        let id = self.id;
        let open = now.duration_since(at) < REQUEST_TIMEOUT && self.watchers.contains(who);
        if !allow || !open {
            return Some(Message::Deny { id });
        }
        let session = random();
        let typist = Typist {
            session,
            seq: 0,
            budget: TYPE_BURST,
            at: now,
        };
        self.typists.insert(who.to_owned(), typist);
        Some(Message::Grant { id, session })
    }

    /// Take typing back from `who`.
    pub fn revoke(&mut self, who: &str) -> Option<Message> {
        self.typists.remove(who)?;
        Some(Message::Revoke { id: self.id })
    }

    /// Someone left the call: they no longer watch or type, and the wait before their
    /// next screen is forgotten.
    pub fn left(&mut self, who: &str) -> Step {
        self.screens.remove(who);
        self.unwatch(who)
    }

    /// Someone stopped watching: they may no longer type either.
    fn unwatch(&mut self, who: &str) -> Step {
        self.watchers.remove(who);
        self.owed.remove(who);
        let asked = self.asking.remove(who).is_some();
        if self.typists.remove(who).is_some() || asked {
            Step::Ended
        } else {
            Step::Nothing
        }
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
    /// What the sharer said about typing.
    Typing(Access),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Access {
    Granted,
    Denied,
    /// Taken back.
    Revoked,
}

/// Asking to type in a terminal we watch, or typing in it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Typing {
    #[default]
    Idle,
    Asking,
    Granted {
        session: Session,
        seq: u64,
    },
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
    pub typing: Typing,
}

impl Viewer {
    pub fn new(sharer: &str, id: Id) -> Self {
        Self {
            sharer: sharer.to_owned(),
            id,
            sync: Sync::Waiting,
            budget: VIEW_BURST,
            at: Instant::now(),
            typing: Typing::Idle,
        }
    }

    /// Ask to type.
    pub fn ask(&mut self) -> Message {
        self.typing = Typing::Asking;
        Message::Request { id: self.id }
    }

    /// Stop asking or typing; `None` if we did neither.
    pub fn release(&mut self) -> Option<Message> {
        let was = std::mem::take(&mut self.typing);
        (was != Typing::Idle).then_some(Message::Withdraw { id: self.id })
    }

    /// What we typed, split into numbered messages, while allowed to type.
    pub fn input(&mut self, bytes: &[u8]) -> Vec<Message> {
        let Typing::Granted { session, seq } = &mut self.typing else {
            return Vec::new();
        };
        bytes
            .chunks(MAX_INPUT)
            .map(|chunk| {
                *seq += 1;
                Message::Input {
                    id: self.id,
                    session: *session,
                    seq: *seq,
                    data: chunk.to_vec(),
                }
            })
            .collect()
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
            Message::End { .. } => {
                self.typing = Typing::Idle;
                Feed::Ended
            }
            Message::Grant { session, .. } if self.typing == Typing::Asking => {
                self.typing = Typing::Granted { session, seq: 0 };
                Feed::Typing(Access::Granted)
            }
            Message::Deny { .. } if self.typing == Typing::Asking => {
                self.typing = Typing::Idle;
                Feed::Typing(Access::Denied)
            }
            Message::Revoke { .. } if matches!(self.typing, Typing::Granted { .. }) => {
                self.typing = Typing::Idle;
                Feed::Typing(Access::Revoked)
            }
            _ => Feed::Nothing,
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
        host.receive("ada", viewer.watch(), true, Instant::now());
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
        host.receive("ada", viewer.watch(), true, Instant::now());
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
        host.receive("bob", Message::Watch { id: [9; 16] }, true, Instant::now());
        assert_eq!(host.watchers().collect::<Vec<_>>(), ["ada"]);
        host.receive(
            "ada",
            Message::Unwatch { id: host.id },
            true,
            Instant::now(),
        );
        assert_eq!(host.watchers().count(), 0);
    }

    #[test]
    fn resync_sends_everyone_a_screen_in_turn() {
        let now = Instant::now();
        let (mut host, _) = watched(now);
        host.receive("bob", Message::Watch { id: host.id }, true, Instant::now());
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
            host.receive("ada", Message::Unwatch { id: host.id }, true, now);
            host.receive("ada", viewer.watch(), true, now);
            assert!(host.due(now).is_empty());
        }
        assert_eq!(host.due(now + SNAPSHOT_EVERY), ["ada"]);
        // Leaving the call forgets it.
        host.left("ada");
        host.receive("ada", viewer.watch(), true, now);
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

    /// Ada watches and asked to type; the sharer allows her.
    fn typing(now: Instant) -> (Host, Viewer) {
        let (mut host, mut viewer) = watched(now);
        assert_eq!(host.receive("ada", viewer.ask(), true, now), Step::Ask);
        assert_eq!(host.askers(now), ["ada"]);
        let grant = host.answer("ada", true, now).unwrap();
        assert_eq!(viewer.receive(SHARER, grant), Feed::Typing(Access::Granted));
        assert_eq!(host.typists().collect::<Vec<_>>(), ["ada"]);
        (host, viewer)
    }

    #[test]
    fn an_allowed_viewer_types_in_order() {
        let now = Instant::now();
        let (mut host, mut viewer) = typing(now);
        let typed = viewer.input(b"ls\r");
        assert_eq!(
            host.receive("ada", typed[0].clone(), true, now),
            Step::Type(b"ls\r".to_vec())
        );
        // A replay is dropped.
        assert_eq!(
            host.receive("ada", typed[0].clone(), true, now),
            Step::Nothing
        );
        // A long paste is split and numbered.
        let paste = viewer.input(&[b'x'; MAX_INPUT + 1]);
        assert_eq!(paste.len(), 2);
        for part in paste {
            assert!(matches!(
                host.receive("ada", part, true, now),
                Step::Type(_)
            ));
        }
    }

    #[test]
    fn input_from_anyone_not_allowed_is_dropped() {
        let now = Instant::now();
        let (mut host, mut viewer) = typing(now);
        let typed = viewer.input(b"whoami\r");
        // Another device, even with Ada's session; and nothing while offline.
        assert_eq!(
            host.receive("bob", typed[0].clone(), true, now),
            Step::Nothing
        );
        assert_eq!(
            host.receive("ada", typed[0].clone(), false, now),
            Step::Nothing
        );
        // A made-up session.
        let id = host.id;
        let forged = |seq| Message::Input {
            id,
            session: [0; 16],
            seq,
            data: b"x".to_vec(),
        };
        assert_eq!(host.receive("ada", forged(99), true, now), Step::Nothing);
        // Taken back, stopped watching or left: nothing more.
        let revoke = host.revoke("ada").unwrap();
        assert_eq!(
            viewer.receive(SHARER, revoke),
            Feed::Typing(Access::Revoked)
        );
        assert!(viewer.input(b"x").is_empty());
        assert_eq!(host.receive("ada", forged(100), true, now), Step::Nothing);
        let (mut host, mut viewer) = typing(now);
        let unwatch = Message::Unwatch { id: host.id };
        assert_eq!(host.receive("ada", unwatch, true, now), Step::Ended);
        let typed = viewer.input(b"x");
        assert_eq!(
            host.receive("ada", typed[0].clone(), true, now),
            Step::Nothing
        );
        let (mut host, mut viewer) = typing(now);
        assert_eq!(host.left("ada"), Step::Ended);
        let typed = viewer.input(b"x");
        assert_eq!(
            host.receive("ada", typed[0].clone(), true, now),
            Step::Nothing
        );
    }

    #[test]
    fn asking_needs_watching_a_connection_and_a_fresh_answer() {
        let now = Instant::now();
        let mut host = Host::default();
        let request = Message::Request { id: host.id };
        let deny = Step::Reply(Message::Deny { id: host.id });
        // Not watching, or the sharer can't tell who asks.
        assert_eq!(host.receive("bob", request.clone(), true, now), deny);
        host.receive("bob", Message::Watch { id: host.id }, true, now);
        assert_eq!(host.receive("bob", request.clone(), false, now), deny);
        assert_eq!(host.receive("bob", request.clone(), true, now), Step::Ask);
        // Too late: the request lapsed, and an "Allow" denies.
        let late = now + REQUEST_TIMEOUT;
        assert!(host.askers(late).is_empty());
        let denied = Some(Message::Deny { id: host.id });
        assert_eq!(host.answer("bob", true, late), denied);
        assert_eq!(host.typists().count(), 0);
        // An answer to nobody does nothing; a withdrawn request can't be allowed.
        assert_eq!(host.answer("bob", true, now), None);
        host.receive("bob", request, true, now);
        let withdraw = Message::Withdraw { id: host.id };
        assert_eq!(host.receive("bob", withdraw, true, now), Step::Ended);
        assert_eq!(host.answer("bob", true, now), None);
    }

    #[test]
    fn typing_is_rate_limited() {
        let now = Instant::now();
        let (mut host, mut viewer) = typing(now);
        let typed: Vec<_> = (0..20)
            .flat_map(|_| viewer.input(&[b'x'; MAX_INPUT]))
            .collect();
        let taken = (typed.into_iter())
            .filter(|message| {
                matches!(
                    host.receive("ada", message.clone(), true, now),
                    Step::Type(_)
                )
            })
            .count();
        assert_eq!(taken, 16);
        // A second later there's room again.
        let later = viewer.input(&[b'x'; MAX_INPUT]);
        let step = host.receive("ada", later[0].clone(), true, now + Duration::from_secs(1));
        assert!(matches!(step, Step::Type(_)));
    }
}
