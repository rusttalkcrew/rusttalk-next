//! A diagram drawn together in a call: the document, the edits to it, the messages
//! between the person who started it (the host) and everyone else in the call, and
//! what each side keeps track of.
//!
//! The host holds the document that counts. Everyone else gets it whole when they
//! join and then the numbered edits the host applied, so a gap is noticed and the
//! whole document asked for again. People the host allowed send their edits to the
//! host, which applies them in the order they arrive and passes them on. Messages
//! travel sealed (see `client::voice`), so the media server can neither read nor
//! forge them.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

/// Random handle for one session, so messages from an earlier one are ignored.
pub type Id = [u8; 16];
/// Random, unguessable handle for the right to edit, given with a grant.
pub type Session = [u8; 16];

/// Shapes and connectors in one diagram.
pub const MAX_ITEMS: usize = 2000;
/// Text in one shape, in bytes.
pub const MAX_TEXT: usize = 1000;
/// Colours to pick from; see the client's palette.
pub const COLORS: u8 = 8;
/// Coordinates and sizes stay within this, so nothing ends up out of reach.
pub const MAX_COORD: f32 = 100_000.0;
pub const MIN_SIZE: f32 = 4.0;
/// JSON bytes of edits or document per message. Sealed, this stays well under
/// LiveKit's data packet limit.
pub const MAX_CHUNK: usize = 8 * 1024;
/// The biggest document a viewer puts together.
pub const MAX_SNAPSHOT: usize = 4 * 1024 * 1024;
/// Each viewer gets the whole document at most this often.
pub const SNAPSHOT_EVERY: Duration = Duration::from_millis(500);
/// A request nobody answered in this time counts as denied.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// After a no, a device's requests are ignored this long; after `MAX_DENIALS` of them,
/// for the rest of the session.
const COOLDOWN: Duration = Duration::from_secs(30);
const MAX_DENIALS: u32 = 3;
/// Edits an editor may make: this many at once, refilled at `EDIT_RATE` per second.
const EDIT_BURST: f64 = 400.0;
const EDIT_RATE: f64 = 50.0;

fn random() -> [u8; 16] {
    let mut bytes = [0; 16];
    crate::identity::random_bytes(&mut bytes).expect("system randomness");
    bytes
}

/// A fresh random ID for a shape or connector, so two editors never pick the same one.
pub fn new_id() -> u64 {
    let bytes = random();
    u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Rect,
    Rounded,
    Ellipse,
    Diamond,
    /// Text without a border or fill.
    Text,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Rect,
        Kind::Rounded,
        Kind::Ellipse,
        Kind::Diamond,
        Kind::Text,
    ];
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shape {
    pub id: u64,
    pub kind: Kind,
    /// The top left corner and size, in diagram units.
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub color: u8,
}

/// An arrow from one shape to another; it follows them when they move.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub id: u64,
    pub from: u64,
    pub to: u64,
    #[serde(default)]
    pub color: u8,
}

fn coord_ok(value: f32) -> bool {
    value.is_finite() && value.abs() <= MAX_COORD
}

/// Text a shape may hold: short, and no control characters but line breaks.
pub fn text_ok(text: &str) -> bool {
    text.len() <= MAX_TEXT && !text.chars().any(|c| c.is_control() && c != '\n')
}

impl Shape {
    pub fn valid(&self) -> bool {
        coord_ok(self.x)
            && coord_ok(self.y)
            && (MIN_SIZE..=MAX_COORD).contains(&self.w)
            && (MIN_SIZE..=MAX_COORD).contains(&self.h)
            && text_ok(&self.text)
            && self.color < COLORS
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Item {
    Shape(Shape),
    Edge(Edge),
}

impl Item {
    pub fn id(&self) -> u64 {
        match self {
            Item::Shape(shape) => shape.id,
            Item::Edge(edge) => edge.id,
        }
    }
}

/// One change to a diagram. Putting an item adds it, or replaces the one with its ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "o", rename_all = "snake_case")]
pub enum Op {
    Put { item: Item },
    Delete { id: u64 },
}

/// A diagram: shapes in drawing order, and the connectors between them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Doc {
    pub shapes: Vec<Shape>,
    pub edges: Vec<Edge>,
}

impl Doc {
    pub fn shape(&self, id: u64) -> Option<&Shape> {
        self.shapes.iter().find(|shape| shape.id == id)
    }

    pub fn edge(&self, id: u64) -> Option<&Edge> {
        self.edges.iter().find(|edge| edge.id == id)
    }

    pub fn item(&self, id: u64) -> Option<Item> {
        (self.shape(id).cloned().map(Item::Shape))
            .or_else(|| self.edge(id).cloned().map(Item::Edge))
    }

    pub fn len(&self) -> usize {
        self.shapes.len() + self.edges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Apply one edit. False, and nothing changes, if it isn't valid here: a bad shape,
    /// a connector to a shape that isn't there, or a full diagram.
    pub fn apply(&mut self, op: &Op) -> bool {
        match op {
            Op::Put {
                item: Item::Shape(shape),
            } => {
                if !shape.valid() || self.edge(shape.id).is_some() {
                    return false;
                }
                let full = self.len() >= MAX_ITEMS;
                match self.shapes.iter_mut().find(|s| s.id == shape.id) {
                    Some(old) => *old = shape.clone(),
                    None if !full => self.shapes.push(shape.clone()),
                    None => return false,
                }
                true
            }
            Op::Put {
                item: Item::Edge(edge),
            } => {
                let ends = edge.from != edge.to
                    && self.shape(edge.from).is_some()
                    && self.shape(edge.to).is_some();
                if !ends || edge.color >= COLORS || self.shape(edge.id).is_some() {
                    return false;
                }
                let full = self.len() >= MAX_ITEMS;
                match self.edges.iter_mut().find(|e| e.id == edge.id) {
                    Some(old) => *old = edge.clone(),
                    None if !full => self.edges.push(edge.clone()),
                    None => return false,
                }
                true
            }
            Op::Delete { id } => {
                let before = self.len();
                self.shapes.retain(|shape| shape.id != *id);
                // Connectors go with either of their shapes.
                self.edges
                    .retain(|edge| edge.id != *id && edge.from != *id && edge.to != *id);
                self.len() != before
            }
        }
    }

    /// The same diagram, keeping only what is valid: what a viewer makes of a
    /// document it was sent, or an imported file.
    pub fn checked(self) -> Doc {
        let mut doc = Doc::default();
        let items = (self.shapes.into_iter().map(Item::Shape))
            .chain(self.edges.into_iter().map(Item::Edge));
        for item in items {
            doc.apply(&Op::Put { item });
        }
        doc
    }
}

/// What a host announces to the call (as a sealed participant attribute) while the
/// session runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Share {
    pub id: Id,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    /// To the host: send me the diagram, and every edit from then on. Also sent again
    /// after missing an edit.
    Join { id: Id },
    /// To the host: stop sending.
    Leave { id: Id },
    /// Host to one viewer: the diagram as of edit `seq`, as JSON. Split over several
    /// messages; the last has `more` false.
    Snapshot {
        id: Id,
        seq: u64,
        #[serde(with = "bytes")]
        data: Vec<u8>,
        more: bool,
    },
    /// Host to viewers: edits that follow `seq - 1`.
    Ops { id: Id, seq: u64, ops: Vec<Op> },
    /// Host to viewers: the session ended.
    End { id: Id },
    /// To the host: may I edit?
    Request { id: Id },
    /// To the host: never mind, or stop editing.
    Withdraw { id: Id },
    /// Host to one viewer: yes; send edits with `session`.
    Grant { id: Id, session: Session },
    /// Host to one viewer: no, or the request lapsed.
    Deny { id: Id },
    /// Host to one viewer: editing taken back.
    Revoke { id: Id },
    /// Editor to host: edits. `seq` rises with every message, so a replay is dropped.
    Edit {
        id: Id,
        session: Session,
        seq: u64,
        ops: Vec<Op>,
    },
}

impl Message {
    pub fn id(&self) -> Id {
        match self {
            Self::Join { id }
            | Self::Leave { id }
            | Self::Snapshot { id, .. }
            | Self::Ops { id, .. }
            | Self::End { id }
            | Self::Request { id }
            | Self::Withdraw { id }
            | Self::Grant { id, .. }
            | Self::Deny { id }
            | Self::Revoke { id }
            | Self::Edit { id, .. } => *id,
        }
    }

    pub fn valid(&self) -> bool {
        match self {
            Self::Snapshot { data, .. } => data.len() <= MAX_CHUNK,
            Self::Ops { ops, .. } | Self::Edit { ops, .. } => {
                !ops.is_empty() && ops.len() <= MAX_CHUNK / 16
            }
            _ => true,
        }
    }
}

/// Edits grouped so each group's JSON fits in one message. A single edit always fits:
/// its text is limited.
pub fn pack(ops: Vec<Op>) -> Vec<Vec<Op>> {
    let mut groups: Vec<Vec<Op>> = Vec::new();
    let mut size = 0;
    for op in ops {
        let len = serde_json::to_vec(&op).map_or(MAX_CHUNK, |json| json.len() + 1);
        match groups.last_mut() {
            Some(group) if size + len <= MAX_CHUNK => {
                group.push(op);
                size += len;
            }
            _ => {
                groups.push(vec![op]);
                size = len;
            }
        }
    }
    groups
}

/// Someone the host let edit.
struct Editor {
    session: Session,
    /// The last `seq` taken, so a replay is dropped.
    seq: u64,
    /// Edits they may still make now, and when that was worked out.
    budget: f64,
    at: Instant,
}

/// What the host does with a message.
#[derive(Debug, PartialEq)]
pub enum Step {
    Nothing,
    /// Show the prompt: someone asks to edit.
    Ask,
    /// Answer the sender with this.
    Reply(Message),
    /// An asker gave up, or someone stopped editing: update the prompt and the names.
    Ended,
    /// An editor's edits, applied: pass them on.
    Applied(Vec<Op>),
}

/// The host's side: the diagram, who follows it, the numbering of the edits, and who
/// may edit.
pub struct Host {
    pub id: Id,
    seq: u64,
    pub doc: Doc,
    members: BTreeSet<String>,
    /// When each device last got the whole diagram.
    snapshots: BTreeMap<String, Instant>,
    /// Members waiting for the whole diagram.
    owed: BTreeSet<String>,
    /// Who asks to edit, since when.
    asking: BTreeMap<String, Instant>,
    editors: BTreeMap<String, Editor>,
    /// Each device's last denial, and how many it had.
    denied: BTreeMap<String, (Instant, u32)>,
}

impl Host {
    pub fn new(doc: Doc) -> Self {
        Self {
            id: random(),
            seq: 0,
            doc: doc.checked(),
            members: BTreeSet::new(),
            snapshots: BTreeMap::new(),
            owed: BTreeSet::new(),
            asking: BTreeMap::new(),
            editors: BTreeMap::new(),
            denied: BTreeMap::new(),
        }
    }

    pub fn members(&self) -> impl Iterator<Item = &str> {
        self.members.iter().map(String::as_str)
    }

    /// Apply edits, the host's own or an editor's; returns those that applied, to pass on.
    pub fn edit(&mut self, ops: Vec<Op>) -> Vec<Op> {
        ops.into_iter().filter(|op| self.doc.apply(op)).collect()
    }

    /// Applied edits as numbered messages for every member.
    pub fn broadcast(&mut self, ops: Vec<Op>) -> Vec<Message> {
        pack(ops)
            .into_iter()
            .map(|ops| {
                self.seq += 1;
                Message::Ops {
                    id: self.id,
                    seq: self.seq,
                    ops,
                }
            })
            .collect()
    }

    /// The whole diagram as of the last edit sent, split into messages that each fit.
    pub fn snapshot(&self) -> Vec<Message> {
        let json = serde_json::to_vec(&self.doc).unwrap_or_default();
        let mut chunks: Vec<&[u8]> = json.chunks(MAX_CHUNK).collect();
        if chunks.is_empty() {
            chunks.push(&[]);
        }
        let last = chunks.len() - 1;
        (chunks.into_iter().enumerate())
            .map(|(at, chunk)| Message::Snapshot {
                id: self.id,
                seq: self.seq,
                data: chunk.to_vec(),
                more: at < last,
            })
            .collect()
    }

    /// A message from someone in the call. Anyone in the call may follow the diagram;
    /// only members may ask to edit, and only those the host allowed may. `online` says
    /// whether the host is connected to the server and so knows who the sender is:
    /// without that nobody may ask or edit.
    pub fn receive(&mut self, from: &str, message: Message, online: bool, now: Instant) -> Step {
        if message.id() != self.id {
            return Step::Nothing;
        }
        let id = self.id;
        match message {
            Message::Join { .. } => {
                self.members.insert(from.to_owned());
                self.owed.insert(from.to_owned());
                Step::Nothing
            }
            Message::Leave { .. } => self.leave(from),
            Message::Request { .. } => {
                if !online || !self.members.contains(from) {
                    return Step::Reply(Message::Deny { id });
                }
                let cooling = self.denied.get(from).is_some_and(|(at, count)| {
                    *count >= MAX_DENIALS || now.duration_since(*at) < COOLDOWN
                });
                if self.editors.contains_key(from) || cooling {
                    return Step::Nothing;
                }
                // Asking again restarts the wait.
                self.asking.insert(from.to_owned(), now);
                Step::Ask
            }
            Message::Withdraw { .. } => {
                let asked = self.asking.remove(from).is_some();
                let edited = self.editors.remove(from).is_some();
                if asked || edited {
                    Step::Ended
                } else {
                    Step::Nothing
                }
            }
            Message::Edit {
                session, seq, ops, ..
            } => {
                let Some(editor) = self.editors.get_mut(from) else {
                    return Step::Nothing;
                };
                if !online || session != editor.session || seq <= editor.seq {
                    return Step::Nothing;
                }
                editor.seq = seq;
                let elapsed = now.saturating_duration_since(editor.at).as_secs_f64();
                editor.budget = (editor.budget + elapsed * EDIT_RATE).min(EDIT_BURST);
                editor.at = now;
                let count = ops.len() as f64;
                if count > editor.budget {
                    // Their copy now differs from ours: send them ours.
                    self.owed.insert(from.to_owned());
                    return Step::Nothing;
                }
                editor.budget -= count;
                let tried = ops.len();
                let applied = self.edit(ops);
                if applied.len() < tried {
                    self.owed.insert(from.to_owned());
                }
                Step::Applied(applied)
            }
            _ => Step::Nothing,
        }
    }

    /// Who asks to edit now, longest waiting first.
    pub fn askers(&self, now: Instant) -> Vec<&str> {
        let mut askers: Vec<_> = (self.asking.iter())
            .filter(|(_, at)| now.duration_since(**at) < REQUEST_TIMEOUT)
            .collect();
        askers.sort_by_key(|(_, at)| **at);
        askers.into_iter().map(|(who, _)| who.as_str()).collect()
    }

    /// Who may edit.
    pub fn editors(&self) -> impl Iterator<Item = &str> {
        self.editors.keys().map(String::as_str)
    }

    /// The host answered `who`'s request. `None` unless that request is still open: an
    /// "Allow" never outlives the request it was meant for.
    pub fn answer(&mut self, who: &str, allow: bool, now: Instant) -> Option<Message> {
        let at = self.asking.remove(who)?;
        let id = self.id;
        let open = now.duration_since(at) < REQUEST_TIMEOUT && self.members.contains(who);
        if !allow || !open {
            let (_, count) = self.denied.get(who).copied().unwrap_or((now, 0));
            self.denied.insert(who.to_owned(), (now, count + 1));
            return Some(Message::Deny { id });
        }
        let session = random();
        let editor = Editor {
            session,
            seq: 0,
            budget: EDIT_BURST,
            at: now,
        };
        self.editors.insert(who.to_owned(), editor);
        Some(Message::Grant { id, session })
    }

    /// Take editing back from `who`.
    pub fn revoke(&mut self, who: &str) -> Option<Message> {
        self.editors.remove(who)?;
        Some(Message::Revoke { id: self.id })
    }

    /// Someone left the call: they no longer follow or edit, and the wait before their
    /// next snapshot is forgotten.
    pub fn left(&mut self, who: &str) -> Step {
        self.snapshots.remove(who);
        self.leave(who)
    }

    fn leave(&mut self, who: &str) -> Step {
        self.members.remove(who);
        self.owed.remove(who);
        let asked = self.asking.remove(who).is_some();
        if self.editors.remove(who).is_some() || asked {
            Step::Ended
        } else {
            Step::Nothing
        }
    }

    /// Members who get the whole diagram now; the others wait until `SNAPSHOT_EVERY`
    /// has passed.
    pub fn due(&mut self, now: Instant) -> Vec<String> {
        let mut due = Vec::new();
        for who in &self.members {
            let last = self.snapshots.get(who);
            let ready = last.is_none_or(|last| now.duration_since(*last) >= SNAPSHOT_EVERY);
            if ready && self.owed.remove(who) {
                self.snapshots.insert(who.clone(), now);
                due.push(who.clone());
            }
        }
        due
    }
}

/// What a viewer does with a message.
#[derive(Clone, Debug, PartialEq)]
pub enum Feed {
    Nothing,
    /// Start over with this diagram.
    Reset(Doc),
    /// Apply these edits, in order.
    Ops(Vec<Op>),
    /// An edit went missing: ask for the diagram again (send `Viewer::join`).
    Resync,
    /// The session ended.
    Ended,
    /// What the host said about editing.
    Editing(Access),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Access {
    Granted,
    Denied,
    /// Taken back.
    Revoked,
}

/// Asking to edit a diagram we follow, or editing it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Editing {
    #[default]
    Idle,
    Asking,
    Granted {
        session: Session,
        seq: u64,
    },
}

enum Sync {
    Waiting,
    Assembling {
        seq: u64,
        data: Vec<u8>,
    },
    /// Up to date with edit `seq`.
    Live(u64),
}

/// The side of someone following a diagram another device hosts.
pub struct Viewer {
    pub host: String,
    pub id: Id,
    sync: Sync,
    pub editing: Editing,
}

impl Viewer {
    pub fn new(host: &str, id: Id) -> Self {
        Self {
            host: host.to_owned(),
            id,
            sync: Sync::Waiting,
            editing: Editing::Idle,
        }
    }

    /// The message that asks for the diagram.
    pub fn join(&self) -> Message {
        Message::Join { id: self.id }
    }

    /// Ask to edit.
    pub fn ask(&mut self) -> Message {
        self.editing = Editing::Asking;
        Message::Request { id: self.id }
    }

    /// Stop asking or editing; `None` if we did neither.
    pub fn release(&mut self) -> Option<Message> {
        let was = std::mem::take(&mut self.editing);
        (was != Editing::Idle).then_some(Message::Withdraw { id: self.id })
    }

    /// Our edits as numbered messages, while allowed to edit.
    pub fn edit(&mut self, ops: Vec<Op>) -> Vec<Message> {
        let Editing::Granted { session, seq } = &mut self.editing else {
            return Vec::new();
        };
        pack(ops)
            .into_iter()
            .map(|ops| {
                *seq += 1;
                Message::Edit {
                    id: self.id,
                    session: *session,
                    seq: *seq,
                    ops,
                }
            })
            .collect()
    }

    /// Handle a message `from` someone; only this session's host counts.
    pub fn receive(&mut self, from: &str, message: Message) -> Feed {
        if from != self.host || message.id() != self.id {
            return Feed::Nothing;
        }
        match message {
            Message::Snapshot {
                seq, data, more, ..
            } => {
                let mut buffer = match std::mem::replace(&mut self.sync, Sync::Waiting) {
                    Sync::Assembling { seq: s, data } if s == seq => data,
                    _ => Vec::new(),
                };
                buffer.extend_from_slice(&data);
                if buffer.len() > MAX_SNAPSHOT {
                    return Feed::Resync;
                }
                if more {
                    self.sync = Sync::Assembling { seq, data: buffer };
                    return Feed::Nothing;
                }
                match serde_json::from_slice::<Doc>(&buffer) {
                    Ok(doc) => {
                        self.sync = Sync::Live(seq);
                        Feed::Reset(doc.checked())
                    }
                    Err(_) => Feed::Resync,
                }
            }
            Message::Ops { seq, ops, .. } => match self.sync {
                Sync::Live(last) if seq == last + 1 => {
                    self.sync = Sync::Live(seq);
                    Feed::Ops(ops)
                }
                // Already in the diagram we got.
                Sync::Live(last) if seq <= last => Feed::Nothing,
                Sync::Live(_) => {
                    self.sync = Sync::Waiting;
                    Feed::Resync
                }
                // Edits for the diagram we're waiting for are in it.
                Sync::Waiting | Sync::Assembling { .. } => Feed::Nothing,
            },
            Message::End { .. } => {
                self.editing = Editing::Idle;
                Feed::Ended
            }
            Message::Grant { session, .. } if self.editing == Editing::Asking => {
                self.editing = Editing::Granted { session, seq: 0 };
                Feed::Editing(Access::Granted)
            }
            Message::Deny { .. } if self.editing == Editing::Asking => {
                self.editing = Editing::Idle;
                Feed::Editing(Access::Denied)
            }
            Message::Revoke { .. } if matches!(self.editing, Editing::Granted { .. }) => {
                self.editing = Editing::Idle;
                Feed::Editing(Access::Revoked)
            }
            _ => Feed::Nothing,
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

    fn shape(id: u64, x: f32) -> Shape {
        Shape {
            id,
            kind: Kind::Rect,
            x,
            y: 0.0,
            w: 120.0,
            h: 60.0,
            text: format!("box {id}"),
            color: 0,
        }
    }

    fn put_shape(id: u64, x: f32) -> Op {
        Op::Put {
            item: Item::Shape(shape(id, x)),
        }
    }

    fn put_edge(id: u64, from: u64, to: u64) -> Op {
        Op::Put {
            item: Item::Edge(Edge {
                id,
                from,
                to,
                color: 0,
            }),
        }
    }

    #[test]
    fn edits_apply_and_bad_ones_change_nothing() {
        let mut doc = Doc::default();
        assert!(doc.apply(&put_shape(1, 0.0)));
        assert!(doc.apply(&put_shape(2, 200.0)));
        assert!(doc.apply(&put_edge(3, 1, 2)));
        // Moving a shape replaces it in place.
        assert!(doc.apply(&put_shape(1, 50.0)));
        assert_eq!(doc.shapes[0].x, 50.0);
        assert_eq!(doc.len(), 3);
        let before = doc.clone();
        // A connector to nothing, to itself, an ID taken by the other kind, bad numbers
        // or text, an unknown colour.
        assert!(!doc.apply(&put_edge(4, 1, 9)));
        assert!(!doc.apply(&put_edge(4, 1, 1)));
        assert!(!doc.apply(&put_edge(1, 1, 2)));
        assert!(!doc.apply(&put_shape(3, 0.0)));
        let mut bad = shape(5, f32::NAN);
        assert!(!doc.apply(&Op::Put {
            item: Item::Shape(bad.clone())
        }));
        bad.x = 0.0;
        bad.w = 1.0;
        assert!(!doc.apply(&Op::Put {
            item: Item::Shape(bad.clone())
        }));
        bad.w = 10.0;
        bad.text = "a\u{1b}[2J".into();
        assert!(!doc.apply(&Op::Put {
            item: Item::Shape(bad.clone())
        }));
        bad.text = "x".repeat(MAX_TEXT + 1);
        assert!(!doc.apply(&Op::Put {
            item: Item::Shape(bad)
        }));
        assert_eq!(doc, before);
        // Deleting a shape takes its connectors with it.
        assert!(doc.apply(&Op::Delete { id: 2 }));
        assert!(doc.edges.is_empty());
        assert!(!doc.apply(&Op::Delete { id: 2 }));
    }

    #[test]
    fn a_full_diagram_takes_nothing_more() {
        let mut doc = Doc::default();
        for id in 0..MAX_ITEMS as u64 {
            assert!(doc.apply(&put_shape(id, 0.0)));
        }
        assert!(!doc.apply(&put_shape(u64::MAX, 0.0)));
        // Changing what is there still works.
        assert!(doc.apply(&put_shape(0, 10.0)));
    }

    /// Two editors change the same shape at once; everyone ends with the host's order.
    #[test]
    fn concurrent_edits_converge_on_the_host_order() {
        let now = Instant::now();
        let mut host = Host::new(Doc::default());
        let start = host.edit(vec![put_shape(1, 0.0)]);
        host.broadcast(start);
        let mut copies = Vec::new();
        for who in ["ada", "bob"] {
            let mut viewer = Viewer::new("host", host.id);
            host.receive(who, viewer.join(), true, now);
            let mut doc = Doc::default();
            for message in host.snapshot() {
                if let Feed::Reset(got) = viewer.receive("host", message) {
                    doc = got;
                }
            }
            host.receive(who, viewer.ask(), true, now);
            let grant = host.answer(who, true, now).unwrap();
            assert_eq!(
                viewer.receive("host", grant),
                Feed::Editing(Access::Granted)
            );
            copies.push((who, viewer, doc));
        }
        // Each moves the shape and sees its own move at once.
        let mut sent = Vec::new();
        for ((who, viewer, doc), x) in copies.iter_mut().zip([100.0, 200.0]) {
            let op = put_shape(1, x);
            doc.apply(&op);
            sent.push((*who, viewer.edit(vec![op])));
        }
        // The host takes Bob's first.
        sent.reverse();
        let mut out = Vec::new();
        for (who, messages) in sent {
            for message in messages {
                let Step::Applied(ops) = host.receive(who, message, true, now) else {
                    panic!("an allowed edit was refused");
                };
                out.extend(host.broadcast(ops));
            }
        }
        for (_, viewer, doc) in &mut copies {
            for message in &out {
                if let Feed::Ops(ops) = viewer.receive("host", message.clone()) {
                    for op in &ops {
                        doc.apply(op);
                    }
                }
            }
        }
        assert_eq!(host.doc.shapes[0].x, 100.0);
        for (_, _, doc) in &copies {
            assert_eq!(*doc, host.doc);
        }
    }

    #[test]
    fn a_late_joiner_gets_the_current_diagram_then_edits() {
        let now = Instant::now();
        let mut host = Host::new(Doc::default());
        // Big enough to take several messages.
        let ops: Vec<Op> = (0..200)
            .map(|id| {
                let mut shape = shape(id, id as f32 * 10.0);
                shape.text = "words ".repeat(50);
                Op::Put {
                    item: Item::Shape(shape),
                }
            })
            .collect();
        let applied = host.edit(ops);
        assert!(host.broadcast(applied).len() > 1);

        let mut viewer = Viewer::new("host", host.id);
        host.receive("late", viewer.join(), true, now);
        assert_eq!(host.due(now), ["late"]);
        // Not again until a while later.
        host.receive("late", viewer.join(), true, now);
        assert!(host.due(now).is_empty());
        assert_eq!(host.due(now + SNAPSHOT_EVERY), ["late"]);

        let parts = host.snapshot();
        assert!(parts.len() > 1);
        // Edits sent before the snapshot is complete are already in it.
        let mut doc = None;
        for part in parts {
            match viewer.receive("host", part) {
                Feed::Reset(got) => doc = Some(got),
                Feed::Nothing => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        let mut doc = doc.unwrap();
        assert_eq!(doc, host.doc);

        let next = host.edit(vec![Op::Delete { id: 3 }]);
        let next = host.broadcast(next);
        for message in next.clone() {
            if let Feed::Ops(ops) = viewer.receive("host", message) {
                ops.iter().for_each(|op| {
                    doc.apply(op);
                });
            }
        }
        assert_eq!(doc, host.doc);
        // A replay is ignored, a gap asks for the diagram again.
        assert_eq!(viewer.receive("host", next[0].clone()), Feed::Nothing);
        let skipped = Message::Ops {
            id: host.id,
            seq: 99,
            ops: vec![Op::Delete { id: 4 }],
        };
        assert_eq!(viewer.receive("host", skipped), Feed::Resync);
        // Only the host speaks for the session.
        let ended = Message::End { id: host.id };
        assert_eq!(viewer.receive("mallory", ended.clone()), Feed::Nothing);
        assert_eq!(viewer.receive("host", ended), Feed::Ended);
    }

    #[test]
    fn only_allowed_members_edit() {
        let now = Instant::now();
        let mut host = Host::new(Doc::default());
        let id = host.id;
        let edit = |session, seq| Message::Edit {
            id,
            session,
            seq,
            ops: vec![put_shape(seq, 0.0)],
        };
        // Not following the diagram, or the host is offline: no.
        assert_eq!(
            host.receive("ada", Message::Request { id }, true, now),
            Step::Reply(Message::Deny { id })
        );
        host.receive("ada", Message::Join { id }, true, now);
        assert_eq!(
            host.receive("ada", Message::Request { id }, false, now),
            Step::Reply(Message::Deny { id })
        );
        assert_eq!(
            host.receive("ada", Message::Request { id }, true, now),
            Step::Ask
        );
        assert_eq!(host.askers(now), ["ada"]);
        // Editing without a grant does nothing.
        assert_eq!(
            host.receive("ada", edit([0; 16], 1), true, now),
            Step::Nothing
        );
        let Some(Message::Grant { session, .. }) = host.answer("ada", true, now) else {
            panic!("no grant");
        };
        assert_eq!(
            host.receive("ada", edit(session, 1), true, now),
            Step::Applied(vec![put_shape(1, 0.0)])
        );
        // A replay, a wrong session, someone else with the session: dropped.
        assert_eq!(
            host.receive("ada", edit(session, 1), true, now),
            Step::Nothing
        );
        assert_eq!(
            host.receive("ada", edit([9; 16], 2), true, now),
            Step::Nothing
        );
        assert_eq!(
            host.receive("bob", edit(session, 2), true, now),
            Step::Nothing
        );
        // Taken back: no more edits.
        assert_eq!(host.revoke("ada"), Some(Message::Revoke { id }));
        assert_eq!(
            host.receive("ada", edit(session, 3), true, now),
            Step::Nothing
        );
        assert_eq!(host.doc.len(), 1);

        // A no, then requests are ignored for a while; an answer after the request
        // lapsed is a no too.
        host.receive("bob", Message::Join { id }, true, now);
        host.receive("bob", Message::Request { id }, true, now);
        assert_eq!(host.answer("bob", false, now), Some(Message::Deny { id }));
        assert_eq!(
            host.receive("bob", Message::Request { id }, true, now),
            Step::Nothing
        );
        let later = now + COOLDOWN;
        assert_eq!(
            host.receive("bob", Message::Request { id }, true, later),
            Step::Ask
        );
        let lapsed = later + REQUEST_TIMEOUT;
        assert_eq!(host.answer("bob", true, lapsed), Some(Message::Deny { id }));
        // Leaving ends editing.
        host.receive("ada", Message::Request { id }, true, lapsed);
        host.answer("ada", true, lapsed);
        assert_eq!(host.left("ada"), Step::Ended);
        assert_eq!(host.editors().count(), 0);
    }

    #[test]
    fn an_editor_whose_edit_was_refused_gets_the_diagram_again() {
        let now = Instant::now();
        let mut host = Host::new(Doc::default());
        let id = host.id;
        host.receive("ada", Message::Join { id }, true, now);
        host.due(now);
        host.receive("ada", Message::Request { id }, true, now);
        let Some(Message::Grant { session, .. }) = host.answer("ada", true, now) else {
            panic!("no grant");
        };
        // A connector to a shape someone else just deleted.
        let message = Message::Edit {
            id,
            session,
            seq: 1,
            ops: vec![put_edge(7, 1, 2)],
        };
        assert_eq!(
            host.receive("ada", message, true, now),
            Step::Applied(vec![])
        );
        assert_eq!(host.due(now + SNAPSHOT_EVERY), ["ada"]);
    }

    #[test]
    fn messages_fit_and_validate() {
        let ops: Vec<Op> = (0..100)
            .map(|id| {
                let mut shape = shape(id, 0.0);
                // The worst case for JSON: every byte escaped.
                shape.text = "\"".repeat(MAX_TEXT);
                Op::Put {
                    item: Item::Shape(shape),
                }
            })
            .collect();
        let mut host = Host::new(Doc::default());
        for message in host.broadcast(ops) {
            assert!(message.valid());
            assert!(serde_json::to_vec(&message).unwrap().len() <= MAX_CHUNK + 256);
            let json = serde_json::to_string(&message).unwrap();
            assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
        }
        let empty = Message::Ops {
            id: [0; 16],
            seq: 1,
            ops: vec![],
        };
        assert!(!empty.valid());
    }
}
