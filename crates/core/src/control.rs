//! Remote control of a shared screen: the messages a viewer and the sharer exchange,
//! what input may be sent, and who the sharer accepts it from.
//!
//! The sharer must approve every request, and only one person controls at a time.
//! Messages travel sealed (see `client::voice`), so the media server can neither read
//! nor forge them; this module decides what a well-formed, expected message does.

use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

/// A request nobody answered in this time counts as denied.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Wheel notches per event, either way.
const MAX_SCROLL: f32 = 10.0;
/// Track IDs are short; anything longer is not one.
const MAX_SCREEN_ID: usize = 64;

/// Random, unguessable handle for one request, and for the session it starts.
pub type Nonce = [u8; 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// One input event. Positions are 0 to 1 across the shared picture, left to right and
/// top to bottom, so they don't depend on either side's screen size.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
pub enum Input {
    Move {
        x: f32,
        y: f32,
    },
    /// Carries its own position: moves are lossy and may arrive after it.
    Button {
        button: Button,
        down: bool,
        x: f32,
        y: f32,
    },
    /// Wheel notches. Positive `y` scrolls up, positive `x` scrolls left.
    Scroll {
        x: f32,
        y: f32,
    },
    /// A physical key, by its position on a US keyboard (see `KEYS`).
    Key {
        key: String,
        down: bool,
    },
}

/// The keys a controller may press, by name, with their PC scan code (set 1; 0xE0xx
/// for extended keys). The Windows key, media keys and Print Screen are left out on
/// purpose, and Ctrl+Alt+Del can't be injected by any app.
pub const KEYS: &[(&str, u16)] = &[
    ("Escape", 0x01),
    ("1", 0x02),
    ("2", 0x03),
    ("3", 0x04),
    ("4", 0x05),
    ("5", 0x06),
    ("6", 0x07),
    ("7", 0x08),
    ("8", 0x09),
    ("9", 0x0A),
    ("0", 0x0B),
    ("Minus", 0x0C),
    ("Equals", 0x0D),
    ("Backspace", 0x0E),
    ("Tab", 0x0F),
    ("Q", 0x10),
    ("W", 0x11),
    ("E", 0x12),
    ("R", 0x13),
    ("T", 0x14),
    ("Y", 0x15),
    ("U", 0x16),
    ("I", 0x17),
    ("O", 0x18),
    ("P", 0x19),
    ("OpenBracket", 0x1A),
    ("CloseBracket", 0x1B),
    ("Enter", 0x1C),
    ("ControlLeft", 0x1D),
    ("A", 0x1E),
    ("S", 0x1F),
    ("D", 0x20),
    ("F", 0x21),
    ("G", 0x22),
    ("H", 0x23),
    ("J", 0x24),
    ("K", 0x25),
    ("L", 0x26),
    ("Semicolon", 0x27),
    ("Quote", 0x28),
    ("Backtick", 0x29),
    ("ShiftLeft", 0x2A),
    ("Backslash", 0x2B),
    ("Z", 0x2C),
    ("X", 0x2D),
    ("C", 0x2E),
    ("V", 0x2F),
    ("B", 0x30),
    ("N", 0x31),
    ("M", 0x32),
    ("Comma", 0x33),
    ("Period", 0x34),
    ("Slash", 0x35),
    ("ShiftRight", 0x36),
    ("AltLeft", 0x38),
    ("Space", 0x39),
    ("CapsLock", 0x3A),
    ("F1", 0x3B),
    ("F2", 0x3C),
    ("F3", 0x3D),
    ("F4", 0x3E),
    ("F5", 0x3F),
    ("F6", 0x40),
    ("F7", 0x41),
    ("F8", 0x42),
    ("F9", 0x43),
    ("F10", 0x44),
    ("IntlBackslash", 0x56),
    ("F11", 0x57),
    ("F12", 0x58),
    ("ControlRight", 0xE01D),
    ("AltRight", 0xE038),
    ("Home", 0xE047),
    ("Up", 0xE048),
    ("PageUp", 0xE049),
    ("Left", 0xE04B),
    ("Right", 0xE04D),
    ("End", 0xE04F),
    ("Down", 0xE050),
    ("PageDown", 0xE051),
    ("Insert", 0xE052),
    ("Delete", 0xE053),
    ("Menu", 0xE05D),
];

/// The scan code of an allowed key.
pub fn scan_code(key: &str) -> Option<u16> {
    KEYS.iter()
        .find(|(name, _)| *name == key)
        .map(|(_, code)| *code)
}

fn unit(value: f32) -> bool {
    (0.0..=1.0).contains(&value)
}

impl Input {
    /// Whether a sharer may act on this at all.
    pub fn valid(&self) -> bool {
        match self {
            Self::Move { x, y } | Self::Button { x, y, .. } => unit(*x) && unit(*y),
            Self::Scroll { x, y } => {
                x.is_finite() && y.is_finite() && x.abs() <= MAX_SCROLL && y.abs() <= MAX_SCROLL
            }
            Self::Key { key, .. } => scan_code(key).is_some(),
        }
    }
}

/// Why a request was not granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// The sharer said no, didn't answer, or doesn't share that screen any more.
    Denied,
    /// Someone else asks or controls already.
    Busy,
    /// The sharer's system can't take remote input.
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Message {
    /// Viewer to sharer: may I control this screen (its track ID)?
    Request {
        request: Nonce,
        screen: String,
    },
    /// Viewer to sharer: never mind, or stop controlling.
    Withdraw {
        request: Nonce,
    },
    /// Sharer to viewer: yes; send input for `session`.
    Grant {
        request: Nonce,
        session: Nonce,
    },
    Deny {
        request: Nonce,
        reason: Refusal,
    },
    /// Sharer to viewer: control taken back.
    Revoke {
        request: Nonce,
    },
    /// Viewer to sharer. `seq` rises with every input, so a replay or a stale move is dropped.
    Input {
        session: Nonce,
        seq: u64,
        input: Input,
    },
}

impl Message {
    /// Moves may be lost or dropped; everything else must arrive, in order.
    pub fn reliable(&self) -> bool {
        !matches!(
            self,
            Self::Input {
                input: Input::Move { .. },
                ..
            }
        )
    }

    pub fn valid(&self) -> bool {
        match self {
            Self::Request { screen, .. } => !screen.is_empty() && screen.len() <= MAX_SCREEN_ID,
            Self::Input { input, .. } => input.valid(),
            _ => true,
        }
    }
}

fn nonce() -> Nonce {
    let mut nonce = [0; 16];
    crate::identity::random_bytes(&mut nonce).expect("system randomness");
    nonce
}

struct Asked {
    from: String,
    request: Nonce,
    at: Instant,
}

struct Active {
    by: String,
    request: Nonce,
    session: Nonce,
    /// Last sequence number seen on the reliable and on the lossy channel. Each is
    /// in order by itself, but not with the other.
    seq: [u64; 2],
    keys: BTreeSet<String>,
    buttons: BTreeSet<Button>,
}

impl Active {
    /// Let go of everything the controller still holds, so nothing stays pressed.
    fn releases(self) -> Vec<Input> {
        let keys = self
            .keys
            .into_iter()
            .map(|key| Input::Key { key, down: false });
        let buttons = self.buttons.into_iter().map(|button| Input::Button {
            button,
            down: false,
            // Not used for a release: the pointer stays where it is.
            x: 0.0,
            y: 0.0,
        });
        keys.chain(buttons).collect()
    }
}

/// What the sharer does with a message.
#[derive(Debug, PartialEq)]
pub enum Step {
    Nothing,
    /// Show the prompt: someone asks for control.
    Ask,
    /// Answer the sender with this.
    Reply(Message),
    /// Inject this input.
    Inject(Input),
    /// The asker gave up or the controller stopped: hide the prompt or the bar, and
    /// inject these releases.
    Ended(Vec<Input>),
}

/// The sharer's side: at most one person asking, or one in control.
#[derive(Default)]
pub struct Host {
    asked: Option<Asked>,
    active: Option<Active>,
}

impl Host {
    /// Who is asking now, if anyone.
    pub fn asker(&self, now: Instant) -> Option<&str> {
        self.asked
            .as_ref()
            .filter(|asked| now.duration_since(asked.at) < REQUEST_TIMEOUT)
            .map(|asked| asked.from.as_str())
    }

    /// The open request, to answer exactly that one.
    pub fn request(&self, now: Instant) -> Option<Nonce> {
        self.asker(now)?;
        self.asked.as_ref().map(|asked| asked.request)
    }

    /// Who controls the screen now, if anyone.
    pub fn controller(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.by.as_str())
    }

    /// Handle a message `from` someone. `screen` is the track we share now, if any;
    /// `supported` says whether this system can inject input into it.
    pub fn receive(
        &mut self,
        from: &str,
        message: Message,
        screen: Option<&str>,
        supported: bool,
        now: Instant,
    ) -> Step {
        match message {
            Message::Request {
                request,
                screen: wanted,
            } => {
                let refuse = |reason| Step::Reply(Message::Deny { request, reason });
                if screen != Some(wanted.as_str()) {
                    return refuse(Refusal::Denied);
                }
                if !supported {
                    return refuse(Refusal::Unsupported);
                }
                if self.active.is_some() || self.asker(now).is_some_and(|asker| asker != from) {
                    return refuse(Refusal::Busy);
                }
                // A new request from the same person replaces their earlier one.
                self.asked = Some(Asked {
                    from: from.to_owned(),
                    request,
                    at: now,
                });
                Step::Ask
            }
            Message::Withdraw { request } => {
                if self
                    .asked
                    .as_ref()
                    .is_some_and(|asked| asked.from == from && asked.request == request)
                {
                    self.asked = None;
                    return Step::Ended(Vec::new());
                }
                if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.by == from && active.request == request)
                {
                    return Step::Ended(
                        self.active.take().map(Active::releases).unwrap_or_default(),
                    );
                }
                Step::Nothing
            }
            Message::Input {
                session,
                seq,
                input,
            } => {
                let Some(active) = self
                    .active
                    .as_mut()
                    .filter(|active| active.by == from && active.session == session)
                else {
                    return Step::Nothing;
                };
                let lane = &mut active.seq[usize::from(matches!(input, Input::Move { .. }))];
                if seq <= *lane || !input.valid() {
                    return Step::Nothing;
                }
                *lane = seq;
                match &input {
                    Input::Key { key, down: true } => {
                        active.keys.insert(key.clone());
                    }
                    Input::Key { key, down: false } => {
                        active.keys.remove(key);
                    }
                    Input::Button { button, down, .. } => {
                        if *down {
                            active.buttons.insert(*button);
                        } else {
                            active.buttons.remove(button);
                        }
                    }
                    _ => {}
                }
                Step::Inject(input)
            }
            // Only a viewer receives these.
            Message::Grant { .. } | Message::Deny { .. } | Message::Revoke { .. } => Step::Nothing,
        }
    }

    /// The sharer answered the prompt for `request`. Returns who asked and what to tell
    /// them, unless that request expired or was withdrawn or replaced meanwhile: an
    /// "Allow" meant for one person never grants another.
    pub fn answer(
        &mut self,
        request: Nonce,
        allow: bool,
        now: Instant,
    ) -> Option<(String, Message)> {
        if self.request(now) != Some(request) {
            return None;
        }
        let asked = self.asked.take()?;
        let request = asked.request;
        if !allow {
            let reason = Refusal::Denied;
            return Some((asked.from, Message::Deny { request, reason }));
        }
        let session = nonce();
        self.active = Some(Active {
            by: asked.from.clone(),
            request,
            session,
            seq: [0; 2],
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
        });
        Some((asked.from, Message::Grant { request, session }))
    }

    /// Take control back, or close an open request: the share stopped, changed, or the
    /// sharer said so. Returns whom to tell, what, and the releases to inject.
    pub fn stop(&mut self) -> Option<(String, Message, Vec<Input>)> {
        if let Some(active) = self.active.take() {
            let to = active.by.clone();
            let request = active.request;
            return Some((to, Message::Revoke { request }, active.releases()));
        }
        let asked = self.asked.take()?;
        let reason = Refusal::Denied;
        let request = asked.request;
        Some((asked.from, Message::Deny { request, reason }, Vec::new()))
    }

    /// Someone left the call. Returns releases to inject if they were the controller,
    /// or `None` if they had nothing to do with this.
    pub fn left(&mut self, who: &str) -> Option<Vec<Input>> {
        if self.asked.as_ref().is_some_and(|asked| asked.from == who) {
            self.asked = None;
            return Some(Vec::new());
        }
        if self.active.as_ref().is_some_and(|active| active.by == who) {
            return self.active.take().map(Active::releases);
        }
        None
    }
}

/// What a viewer learns from a message.
#[derive(Debug, PartialEq)]
pub enum Answer {
    Nothing,
    Granted,
    Refused(Refusal),
    /// The sharer took control back.
    Revoked,
}

/// The viewer's side: asking for, or controlling, one screen.
#[derive(Default)]
pub enum Guest {
    #[default]
    Idle,
    Asking {
        sharer: String,
        screen: String,
        request: Nonce,
    },
    Controlling {
        sharer: String,
        screen: String,
        request: Nonce,
        session: Nonce,
        seq: u64,
    },
}

impl Guest {
    /// The screen asked for or controlled, and whether control was granted.
    pub fn screen(&self) -> Option<(&str, bool)> {
        match self {
            Self::Idle => None,
            Self::Asking { screen, .. } => Some((screen, false)),
            Self::Controlling { screen, .. } => Some((screen, true)),
        }
    }

    pub fn sharer(&self) -> Option<&str> {
        match self {
            Self::Idle => None,
            Self::Asking { sharer, .. } | Self::Controlling { sharer, .. } => Some(sharer),
        }
    }

    /// Ask `sharer` for control of `screen`. Release any earlier one first.
    pub fn ask(&mut self, sharer: &str, screen: &str) -> Message {
        let request = nonce();
        *self = Self::Asking {
            sharer: sharer.to_owned(),
            screen: screen.to_owned(),
            request,
        };
        Message::Request {
            request,
            screen: screen.to_owned(),
        }
    }

    /// Stop asking or controlling. Returns whom to tell, and what.
    pub fn release(&mut self) -> Option<(String, Message)> {
        match std::mem::take(self) {
            Self::Idle => None,
            Self::Asking {
                sharer, request, ..
            }
            | Self::Controlling {
                sharer, request, ..
            } => Some((sharer, Message::Withdraw { request })),
        }
    }

    /// Handle a message `from` someone; only the sharer asked, about this request, counts.
    pub fn receive(&mut self, from: &str, message: &Message) -> Answer {
        let (sharer, asked) = match self {
            Self::Idle => return Answer::Nothing,
            Self::Asking {
                sharer, request, ..
            }
            | Self::Controlling {
                sharer, request, ..
            } => (sharer.as_str(), *request),
        };
        if sharer != from {
            return Answer::Nothing;
        }
        match (&*self, message) {
            (Self::Asking { sharer, screen, .. }, Message::Grant { request, session })
                if *request == asked =>
            {
                *self = Self::Controlling {
                    sharer: sharer.clone(),
                    screen: screen.clone(),
                    request: asked,
                    session: *session,
                    seq: 0,
                };
                Answer::Granted
            }
            (Self::Asking { .. }, Message::Deny { request, reason }) if *request == asked => {
                *self = Self::Idle;
                Answer::Refused(*reason)
            }
            (Self::Controlling { .. }, Message::Revoke { request }) if *request == asked => {
                *self = Self::Idle;
                Answer::Revoked
            }
            _ => Answer::Nothing,
        }
    }

    /// Wrap input for the sharer, while in control. Returns whom to send it to.
    pub fn input(&mut self, input: Input) -> Option<(String, Message)> {
        let Self::Controlling {
            sharer,
            session,
            seq,
            ..
        } = self
        else {
            return None;
        };
        if !input.valid() {
            return None;
        }
        *seq += 1;
        let message = Message::Input {
            session: *session,
            seq: *seq,
            input,
        };
        Some((sharer.clone(), message))
    }

    /// The sharer left, or stopped sharing `screen`: control is over. True if it was ours.
    pub fn gone(&mut self, sharer: Option<&str>, screen: Option<&str>) -> bool {
        let ours = match self {
            Self::Idle => false,
            Self::Asking {
                sharer: s,
                screen: t,
                ..
            }
            | Self::Controlling {
                sharer: s,
                screen: t,
                ..
            } => sharer == Some(s.as_str()) || screen == Some(t.as_str()),
        };
        if ours {
            *self = Self::Idle;
        }
        ours
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADA: &str = "ada-device";
    const BOB: &str = "bob-device";
    const SCREEN: &str = "TR_screen";

    fn key(key: &str, down: bool) -> Input {
        Input::Key {
            key: key.into(),
            down,
        }
    }

    /// The sharer grants Ada; returns the viewer side, now in control.
    fn granted(host: &mut Host, now: Instant) -> Guest {
        let mut guest = Guest::default();
        let request = guest.ask("sharer", SCREEN);
        assert_eq!(
            host.receive(ADA, request, Some(SCREEN), true, now),
            Step::Ask
        );
        let open = host.request(now).unwrap();
        let (to, grant) = host.answer(open, true, now).unwrap();
        assert_eq!(to, ADA);
        assert_eq!(guest.receive("sharer", &grant), Answer::Granted);
        guest
    }

    fn send(host: &mut Host, guest: &mut Guest, input: Input, now: Instant) -> Step {
        let (_, message) = guest.input(input).unwrap();
        host.receive(ADA, message, Some(SCREEN), true, now)
    }

    #[test]
    fn messages_round_trip_and_validate() {
        let messages = [
            Message::Request {
                request: [1; 16],
                screen: SCREEN.into(),
            },
            Message::Grant {
                request: [1; 16],
                session: [2; 16],
            },
            Message::Deny {
                request: [1; 16],
                reason: Refusal::Unsupported,
            },
            Message::Input {
                session: [2; 16],
                seq: 7,
                input: Input::Button {
                    button: Button::Right,
                    down: true,
                    x: 0.25,
                    y: 1.0,
                },
            },
            Message::Input {
                session: [2; 16],
                seq: 8,
                input: key("ControlLeft", true),
            },
        ];
        for message in messages {
            let wire = serde_json::to_vec(&message).unwrap();
            let back: Message = serde_json::from_slice(&wire).unwrap();
            assert_eq!(back, message);
            assert!(back.valid());
        }
        let input = |input| Message::Input {
            session: [0; 16],
            seq: 1,
            input,
        };
        assert!(!input(Input::Move { x: 1.5, y: 0.0 }).valid());
        assert!(
            !input(Input::Move {
                x: f32::NAN,
                y: 0.0
            })
            .valid()
        );
        assert!(!input(Input::Scroll { x: 0.0, y: 11.0 }).valid());
        assert!(
            !input(Input::Scroll {
                x: f32::INFINITY,
                y: 0.0
            })
            .valid()
        );
        assert!(!input(key("SuperLeft", true)).valid(), "no Windows key");
        assert!(!input(key("", true)).valid());
        assert!(input(Input::Scroll { x: -2.5, y: 10.0 }).valid());
        let long = Message::Request {
            request: [0; 16],
            screen: "x".repeat(65),
        };
        assert!(!long.valid());
        // Unknown kinds and fields that don't fit are rejected while decoding.
        assert!(serde_json::from_str::<Message>(r#"{"t":"grant","request":[1]}"#).is_err());
        assert!(serde_json::from_str::<Message>(r#"{"t":"reboot"}"#).is_err());
        // Only moves may be lost.
        assert!(!input(Input::Move { x: 0.5, y: 0.5 }).reliable());
        assert!(input(key("A", true)).reliable());
        assert_eq!(scan_code("Delete"), Some(0xE053));
    }

    #[test]
    fn request_allow_input_and_revoke() {
        let now = Instant::now();
        let mut host = Host::default();
        let mut guest = granted(&mut host, now);
        assert_eq!(host.controller(), Some(ADA));
        assert_eq!(guest.screen(), Some((SCREEN, true)));
        let step = send(&mut host, &mut guest, key("ShiftLeft", true), now);
        assert_eq!(step, Step::Inject(key("ShiftLeft", true)));
        let press = Input::Button {
            button: Button::Left,
            down: true,
            x: 0.5,
            y: 0.5,
        };
        assert_eq!(
            send(&mut host, &mut guest, press.clone(), now),
            Step::Inject(press)
        );
        // Taking control back lets go of what is still held.
        let (to, revoke, releases) = host.stop().unwrap();
        assert_eq!(to, ADA);
        assert_eq!(releases.len(), 2);
        assert!(releases.contains(&key("ShiftLeft", false)));
        assert_eq!(guest.receive("sharer", &revoke), Answer::Revoked);
        assert_eq!(guest.screen(), None);
        assert!(guest.input(key("A", true)).is_none());
        assert!(host.controller().is_none() && host.stop().is_none());
    }

    #[test]
    fn deny_timeout_and_withdraw() {
        let now = Instant::now();
        let mut host = Host::default();
        let mut guest = Guest::default();
        let request = guest.ask("sharer", SCREEN);
        host.receive(ADA, request.clone(), Some(SCREEN), true, now);
        assert_eq!(host.asker(now), Some(ADA));
        // An answer to an earlier request does nothing; the prompt stays.
        assert!(host.answer([0; 16], true, now).is_none());
        assert_eq!(host.asker(now), Some(ADA));
        let open = host.request(now).unwrap();
        let (_, deny) = host.answer(open, false, now).unwrap();
        assert_eq!(
            guest.receive("sharer", &deny),
            Answer::Refused(Refusal::Denied)
        );
        assert!(host.controller().is_none());

        // Unanswered: the prompt goes away and a late "Allow" grants nothing.
        let request = guest.ask("sharer", SCREEN);
        host.receive(ADA, request, Some(SCREEN), true, now);
        let open = host.request(now).unwrap();
        let later = now + REQUEST_TIMEOUT;
        assert_eq!(host.asker(later), None);
        assert!(host.answer(open, true, later).is_none());
        assert!(host.controller().is_none());

        // Withdrawn while asking: the prompt closes.
        let request = guest.ask("sharer", SCREEN);
        host.receive(ADA, request, Some(SCREEN), true, now);
        let open = host.request(now).unwrap();
        let (_, withdraw) = guest.release().unwrap();
        assert_eq!(
            host.receive(ADA, withdraw, Some(SCREEN), true, now),
            Step::Ended(Vec::new())
        );
        assert!(host.answer(open, true, now).is_none());

        // Withdrawn while controlling: control ends and held keys are released.
        let mut guest = granted(&mut host, now);
        send(&mut host, &mut guest, key("A", true), now);
        let (_, withdraw) = guest.release().unwrap();
        assert_eq!(
            host.receive(ADA, withdraw, Some(SCREEN), true, now),
            Step::Ended(vec![key("A", false)])
        );
        assert!(host.controller().is_none());
    }

    #[test]
    fn refusals_without_a_prompt() {
        let now = Instant::now();
        let mut host = Host::default();
        let ask = |screen: &str| Message::Request {
            request: [3; 16],
            screen: screen.into(),
        };
        let deny = |reason| {
            Step::Reply(Message::Deny {
                request: [3; 16],
                reason,
            })
        };
        // Not what we share, or nothing shared.
        assert_eq!(
            host.receive(BOB, ask("other"), Some(SCREEN), true, now),
            deny(Refusal::Denied)
        );
        assert_eq!(
            host.receive(BOB, ask(SCREEN), None, true, now),
            deny(Refusal::Denied)
        );
        assert_eq!(
            host.receive(BOB, ask(SCREEN), Some(SCREEN), false, now),
            deny(Refusal::Unsupported)
        );
        // One at a time: Bob is turned away while Ada asks, and while she controls.
        host.receive(ADA, ask(SCREEN), Some(SCREEN), true, now);
        assert_eq!(
            host.receive(BOB, ask(SCREEN), Some(SCREEN), true, now),
            deny(Refusal::Busy)
        );
        host.answer([3; 16], true, now).unwrap();
        assert_eq!(
            host.receive(BOB, ask(SCREEN), Some(SCREEN), true, now),
            deny(Refusal::Busy)
        );
        assert_eq!(host.controller(), Some(ADA));
        // Once Ada's request expired, Bob may ask, and Allow meant for Ada can't grant him.
        let mut host = Host::default();
        host.receive(ADA, ask(SCREEN), Some(SCREEN), true, now);
        let later = now + REQUEST_TIMEOUT;
        let bob = Message::Request {
            request: [4; 16],
            screen: SCREEN.into(),
        };
        assert_eq!(host.receive(BOB, bob, Some(SCREEN), true, later), Step::Ask);
        assert_eq!(host.asker(later), Some(BOB));
        assert!(host.answer([3; 16], true, later).is_none());
        assert!(host.controller().is_none());
        assert_eq!(host.request(later), Some([4; 16]));
    }

    #[test]
    fn input_from_anyone_else_is_ignored() {
        let now = Instant::now();
        let mut host = Host::default();
        let mut guest = granted(&mut host, now);
        let (_, message) = guest.input(key("A", true)).unwrap();
        // Bob replays Ada's input under his own identity: ignored.
        assert_eq!(
            host.receive(BOB, message.clone(), Some(SCREEN), true, now),
            Step::Nothing
        );
        assert!(matches!(
            host.receive(ADA, message.clone(), Some(SCREEN), true, now),
            Step::Inject(_)
        ));
        // The same message again, or an older one, is a replay.
        assert_eq!(
            host.receive(ADA, message, Some(SCREEN), true, now),
            Step::Nothing
        );
        // A forged session, or an invalid event, is dropped.
        let forged = Message::Input {
            session: [9; 16],
            seq: 100,
            input: key("B", true),
        };
        assert_eq!(
            host.receive(ADA, forged, Some(SCREEN), true, now),
            Step::Nothing
        );
        let Some((_, Message::Input { session, .. })) = guest.input(key("A", false)) else {
            panic!("expected input");
        };
        let bad = Message::Input {
            session,
            seq: 200,
            input: key("SuperLeft", true),
        };
        assert_eq!(
            host.receive(ADA, bad, Some(SCREEN), true, now),
            Step::Nothing
        );
        // Moves and keys keep separate order: a key sent after a move that arrives
        // before it still counts, but a stale move does not.
        let (_, early) = guest.input(Input::Move { x: 0.1, y: 0.1 }).unwrap();
        let (_, late) = guest.input(Input::Move { x: 0.2, y: 0.2 }).unwrap();
        let (_, enter) = guest.input(key("Enter", true)).unwrap();
        assert!(matches!(
            host.receive(ADA, late, Some(SCREEN), true, now),
            Step::Inject(_)
        ));
        assert!(matches!(
            host.receive(ADA, enter, Some(SCREEN), true, now),
            Step::Inject(_)
        ));
        assert_eq!(
            host.receive(ADA, early, Some(SCREEN), true, now),
            Step::Nothing
        );
        // A grant meant for Ada means nothing coming from Bob, or for another request.
        let mut other = Guest::default();
        let Message::Request { request, .. } = other.ask("sharer", SCREEN) else {
            panic!("expected request");
        };
        let session = [1; 16];
        assert_eq!(
            other.receive(BOB, &Message::Grant { request, session }),
            Answer::Nothing
        );
        let stale = Message::Grant {
            request: [0; 16],
            session,
        };
        assert_eq!(other.receive("sharer", &stale), Answer::Nothing);
        assert_eq!(other.screen(), Some((SCREEN, false)));
    }

    #[test]
    fn leaving_ends_control() {
        let now = Instant::now();
        let mut host = Host::default();
        let mut guest = granted(&mut host, now);
        send(&mut host, &mut guest, key("ControlLeft", true), now);
        assert_eq!(host.left(BOB), None);
        assert_eq!(host.left(ADA), Some(vec![key("ControlLeft", false)]));
        assert!(host.controller().is_none());
        // The viewer stops when the sharer leaves or stops sharing.
        assert!(!guest.gone(Some(BOB), Some("other")));
        assert!(guest.gone(None, Some(SCREEN)));
        assert!(guest.release().is_none());
    }
}
