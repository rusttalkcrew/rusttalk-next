//! Asking someone to join your voice channel (#294): what rings, until when, and who
//! is muted. Requests and answers are encrypted DM messages, like pokes; all of this
//! lives in memory only.

use std::collections::{HashMap, HashSet, VecDeque};

use rusttalk_core::messages::VoiceAnswer;
use url::Url;

use crate::server::Id;

/// How long a request rings, in seconds.
pub const SHOWN: i64 = 30;
/// Time for an answer given in the last second to arrive.
const GRACE: i64 = 5;
/// How long the asker sees the outcome.
const NOTICE: i64 = 6;
pub const MUTE: i64 = 15 * 60;
/// Requests taken from one person per `WINDOW` seconds; more are dropped unseen.
const LIMIT: usize = 3;
const WINDOW: i64 = 60;

/// A request that rings here.
#[derive(Clone)]
pub struct Incoming {
    /// The request's message ID.
    pub id: Id,
    pub person: Id,
    pub name: String,
    pub server: Url,
    pub channel: Id,
    pub channel_name: String,
    pub until: i64,
}

/// A request from here, then its outcome.
pub struct Outgoing {
    pub id: Id,
    pub person: Id,
    pub name: String,
    pub state: State,
    until: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    Answered(VoiceAnswer),
    NoAnswer,
}

#[derive(Default)]
pub struct Requests {
    pub incoming: Vec<Incoming>,
    pub outgoing: Vec<Outgoing>,
    muted: HashMap<Id, i64>,
    seen: HashSet<(Id, Id)>,
    /// When requests were taken from each person lately, for `LIMIT`.
    recent: HashMap<Id, VecDeque<i64>>,
    /// The request on top and since when, in the window's clock.
    top: Option<(Id, f64)>,
}

impl Requests {
    /// Let a request sent at `at` (server time) ring, unless it is old, known, muted
    /// or one too many. A person's newer request replaces their older one.
    pub fn take(&mut self, mut request: Incoming, at: i64, now: i64) -> bool {
        let person = request.person;
        if now - at > SHOWN {
            tracing::info!(age = now - at, "voice request dropped: too old to ring");
            return false;
        }
        if self.muted(&person, now) || !self.seen.insert((person, request.id)) {
            return false;
        }
        let recent = self.recent.entry(person).or_default();
        recent.retain(|taken| now - taken < WINDOW);
        if recent.len() >= LIMIT {
            tracing::debug!("voice request dropped: too many from one person");
            return false;
        }
        recent.push_back(now);
        request.until = now + SHOWN;
        self.incoming.retain(|old| old.person != person);
        self.incoming.push(request);
        true
    }

    pub fn muted(&self, person: &Id, now: i64) -> bool {
        self.muted.get(person).is_some_and(|until| *until > now)
    }

    pub fn mute(&mut self, person: Id, now: i64) {
        self.muted.insert(person, now + MUTE);
        self.incoming.retain(|request| request.person != person);
    }

    /// This request was answered here or on another of this person's devices.
    pub fn close(&mut self, person: &Id, id: &Id) {
        self.incoming
            .retain(|request| request.person != *person || request.id != *id);
    }

    pub fn waiting(&self, person: &Id) -> bool {
        (self.outgoing.iter()).any(|sent| sent.person == *person && sent.state == State::Waiting)
    }

    pub fn asked(&mut self, id: Id, person: Id, name: String, now: i64) {
        self.outgoing.retain(|sent| sent.person != person);
        self.outgoing.push(Outgoing {
            id,
            person,
            name,
            state: State::Waiting,
            until: now + SHOWN + GRACE,
        });
    }

    /// An answer counts once, and only from the person who was asked.
    pub fn answered(&mut self, person: &Id, id: &Id, answer: VoiceAnswer, now: i64) {
        let asked = (self.outgoing.iter_mut())
            .find(|sent| sent.person == *person && sent.id == *id && sent.state == State::Waiting);
        if let Some(sent) = asked {
            sent.state = State::Answered(answer);
            sent.until = now + NOTICE;
        }
    }

    pub fn expire(&mut self, now: i64) {
        self.incoming.retain(|request| request.until > now);
        for sent in &mut self.outgoing {
            if sent.state == State::Waiting && sent.until <= now {
                sent.state = State::NoAnswer;
                sent.until = now + NOTICE;
            }
        }
        self.outgoing.retain(|sent| sent.until > now);
    }

    /// Since when request `id` is on top. The next one slides under the pointer when
    /// one closes, and its Join opens the microphone to someone else.
    pub fn on_top_since(&mut self, id: Id, time: f64) -> f64 {
        match self.top {
            Some((top, since)) if top == id => since,
            _ => {
                self.top = Some((id, time));
                time
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANN: Id = [7; 32];
    const BEN: Id = [8; 32];

    fn request(id: u8, person: Id) -> Incoming {
        Incoming {
            id: [id; 32],
            person,
            name: String::new(),
            server: "https://friends.example.org/".parse().unwrap(),
            channel: [2; 32],
            channel_name: String::new(),
            until: 0,
        }
    }

    #[test]
    fn old_repeated_excess_and_muted_requests_do_not_ring() {
        let mut requests = Requests::default();
        assert!(!requests.take(request(1, ANN), 100, 100 + SHOWN + 1));
        assert!(requests.take(request(1, ANN), 100, 100 + SHOWN));
        assert!(!requests.take(request(1, ANN), 130, 130));
        assert!(requests.take(request(2, ANN), 131, 131));
        assert!(requests.take(request(3, ANN), 132, 132));
        assert_eq!(requests.incoming.len(), 1);
        assert!(!requests.take(request(4, ANN), 133, 133));
        assert!(requests.take(request(5, ANN), 200, 200));
        assert!(requests.take(request(6, BEN), 200, 200));

        requests.mute(ANN, 300);
        assert!(
            requests
                .incoming
                .iter()
                .all(|request| request.person == BEN)
        );
        assert!(!requests.take(request(7, ANN), 299 + MUTE, 299 + MUTE));
        assert!(requests.take(request(8, ANN), 300 + MUTE, 300 + MUTE));
    }

    #[test]
    fn only_the_person_asked_answers_and_silence_becomes_no_answer() {
        let mut requests = Requests::default();
        let (asked, other) = ([1; 32], [2; 32]);
        requests.asked(asked, ANN, String::new(), 100);
        requests.answered(&BEN, &asked, VoiceAnswer::Joining, 101);
        requests.answered(&ANN, &other, VoiceAnswer::Joining, 101);
        assert!(requests.waiting(&ANN));
        requests.answered(&ANN, &asked, VoiceAnswer::Later, 102);
        requests.answered(&ANN, &asked, VoiceAnswer::Joining, 103);
        assert_eq!(
            requests.outgoing[0].state,
            State::Answered(VoiceAnswer::Later)
        );

        requests.asked(other, BEN, String::new(), 200);
        requests.expire(200 + SHOWN + GRACE);
        assert_eq!(requests.outgoing[0].state, State::NoAnswer);
        requests.expire(200 + SHOWN + GRACE + NOTICE);
        assert!(requests.outgoing.is_empty());
    }
}
