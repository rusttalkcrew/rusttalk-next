//! Polls: a poll message, votes that point at it, and its end, all inside the
//! encrypted body. Readers count the votes themselves; the server only sees ciphertext.

use std::collections::HashMap;

use super::MessageError;

/// Answer options per poll.
pub const MIN_POLL_OPTIONS: usize = 2;
pub const MAX_POLL_OPTIONS: usize = 10;
/// Question length in Unicode scalar values.
pub const MAX_POLL_QUESTION: usize = 300;
/// One option's length in Unicode scalar values.
pub const MAX_POLL_OPTION: usize = 100;
/// The poll section at its largest: kind, flag, end time, count and every option.
pub(super) const MAX_BYTES: usize = 1 + 1 + 8 + 1 + MAX_POLL_OPTIONS * (2 + MAX_POLL_OPTION * 4);

const OPEN: u8 = 1;
const VOTE: u8 = 2;
const END: u8 = 3;

/// A poll, a vote or a poll's end, sent instead of chat text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollPart {
    /// A new poll; the message text is its question.
    Open(Poll),
    /// One person's choices (option indexes, ascending), replacing their earlier
    /// vote; none takes it back. `sent` is the sender's clock in milliseconds, so
    /// the latest vote wins whatever order a server returns them in.
    Vote {
        poll: [u8; 32],
        sent: u64,
        choices: Vec<u8>,
    },
    /// Ends a poll early. Only its creator's counts.
    End { poll: [u8; 32] },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Poll {
    pub options: Vec<String>,
    pub multiple: bool,
    /// When voting closes, in unix seconds of server time; 0 for never.
    pub ends_at: i64,
}

fn valid_option(option: &str) -> bool {
    !option.trim().is_empty() && option.chars().count() <= MAX_POLL_OPTION
}

/// Whether a question and options make a poll that can be sent.
pub fn valid_poll(question: &str, options: &[String]) -> bool {
    !question.trim().is_empty()
        && question.chars().count() <= MAX_POLL_QUESTION
        && (MIN_POLL_OPTIONS..=MAX_POLL_OPTIONS).contains(&options.len())
        && options.iter().all(|option| valid_option(option))
}

/// Ascending without repeats, and each an index a poll could have.
fn valid_choices(choices: &[u8]) -> bool {
    choices.len() <= MAX_POLL_OPTIONS
        && choices.windows(2).all(|pair| pair[0] < pair[1])
        && choices.iter().all(|&c| (c as usize) < MAX_POLL_OPTIONS)
}

impl PollPart {
    /// The text that goes with it: the question for a poll, nothing for the rest.
    pub(super) fn check_text(&self, text: &str) -> Result<(), MessageError> {
        let ok = match self {
            Self::Open(poll) => valid_poll(text, &poll.options),
            _ => text.is_empty(),
        };
        ok.then_some(()).ok_or(MessageError::Encoding)
    }

    pub(super) fn encode(&self, body: &mut Vec<u8>) -> Result<(), MessageError> {
        match self {
            Self::Open(poll) => {
                body.extend_from_slice(&[OPEN, poll.multiple as u8]);
                body.extend_from_slice(&poll.ends_at.to_be_bytes());
                body.push(poll.options.len() as u8);
                for option in &poll.options {
                    body.extend_from_slice(&(option.len() as u16).to_be_bytes());
                    body.extend_from_slice(option.as_bytes());
                }
            }
            Self::Vote {
                poll,
                sent,
                choices,
            } => {
                if !valid_choices(choices) {
                    return Err(MessageError::Encoding);
                }
                body.push(VOTE);
                body.extend_from_slice(poll);
                body.extend_from_slice(&sent.to_be_bytes());
                body.push(choices.len() as u8);
                body.extend_from_slice(choices);
            }
            Self::End { poll } => {
                body.push(END);
                body.extend_from_slice(poll);
            }
        }
        Ok(())
    }

    pub(super) fn decode(tail: &mut &[u8]) -> Result<Self, MessageError> {
        let mut next = |n: usize| take(tail, n);
        let id = |bytes: &[u8]| -> [u8; 32] { bytes.try_into().expect("32 bytes") };
        Ok(match next(1)?[0] {
            OPEN => {
                let multiple = match next(1)?[0] {
                    0 => false,
                    1 => true,
                    _ => return Err(MessageError::Encoding),
                };
                let ends_at = i64::from_be_bytes(next(8)?.try_into().expect("8 bytes"));
                let count = next(1)?[0] as usize;
                if ends_at < 0 || !(MIN_POLL_OPTIONS..=MAX_POLL_OPTIONS).contains(&count) {
                    return Err(MessageError::Encoding);
                }
                let mut options = Vec::with_capacity(count);
                for _ in 0..count {
                    let length = u16::from_be_bytes(next(2)?.try_into().expect("2 bytes"));
                    if length as usize > MAX_POLL_OPTION * 4 {
                        return Err(MessageError::Encoding);
                    }
                    let option = std::str::from_utf8(next(length as usize)?)
                        .map_err(|_| MessageError::Encoding)?;
                    if !valid_option(option) {
                        return Err(MessageError::Encoding);
                    }
                    options.push(option.to_owned());
                }
                Self::Open(Poll {
                    options,
                    multiple,
                    ends_at,
                })
            }
            VOTE => {
                let poll = id(next(32)?);
                let sent = u64::from_be_bytes(next(8)?.try_into().expect("8 bytes"));
                let count = next(1)?[0] as usize;
                if count > MAX_POLL_OPTIONS {
                    return Err(MessageError::Encoding);
                }
                let choices = next(count)?.to_vec();
                if !valid_choices(&choices) {
                    return Err(MessageError::Encoding);
                }
                Self::Vote {
                    poll,
                    sent,
                    choices,
                }
            }
            END => Self::End {
                poll: id(next(32)?),
            },
            _ => return Err(MessageError::Encoding),
        })
    }
}

fn take<'a>(tail: &mut &'a [u8], n: usize) -> Result<&'a [u8], MessageError> {
    let (head, rest) = tail.split_at_checked(n).ok_or(MessageError::Encoding)?;
    *tail = rest;
    Ok(head)
}

/// A vote as received: who sent it, when the server took it, and its record ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ballot {
    pub person: [u8; 32],
    /// Server time in unix seconds.
    pub at: i64,
    pub sent: u64,
    pub id: [u8; 32],
    pub choices: Vec<u8>,
}

/// A poll's results.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Each option's voters, in the order of their votes' server times.
    pub voters: Vec<Vec<[u8; 32]>>,
    /// People with at least one choice.
    pub people: usize,
    /// When voting closed or closes: the end time, or the creator's earlier end.
    pub closes_at: Option<i64>,
    /// The creator ended it early.
    pub ended: bool,
}

impl Tally {
    pub fn open(&self, now: i64) -> bool {
        !self.ended && self.closes_at.is_none_or(|at| now < at)
    }
}

/// Count a poll's votes. Ends from anyone but the creator, votes after the close,
/// votes with an index out of range or several choices on a single-answer poll are
/// ignored. Each person's latest valid vote counts.
pub fn tally(
    poll: &Poll,
    creator: &[u8; 32],
    ballots: &[Ballot],
    ends: &[([u8; 32], i64)],
) -> Tally {
    let ended_at = (ends.iter())
        .filter(|(person, _)| person == creator)
        .map(|(_, at)| *at)
        .min();
    let deadline = (poll.ends_at > 0).then_some(poll.ends_at);
    let closes_at = match (deadline, ended_at) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let mut latest: HashMap<[u8; 32], &Ballot> = HashMap::new();
    for ballot in ballots {
        let in_range = ballot
            .choices
            .iter()
            .all(|&choice| (choice as usize) < poll.options.len());
        if !in_range
            || (!poll.multiple && ballot.choices.len() > 1)
            || closes_at.is_some_and(|at| ballot.at > at)
        {
            continue;
        }
        let newer =
            |old: &&Ballot| (old.sent, old.at, old.id) < (ballot.sent, ballot.at, ballot.id);
        if latest.get(&ballot.person).is_none_or(newer) {
            latest.insert(ballot.person, ballot);
        }
    }
    let mut counted: Vec<&Ballot> = latest.into_values().collect();
    counted.sort_by_key(|ballot| (ballot.at, ballot.id));
    let mut voters = vec![Vec::new(); poll.options.len()];
    for ballot in &counted {
        for &choice in &ballot.choices {
            voters[choice as usize].push(ballot.person);
        }
    }
    Tally {
        voters,
        people: counted.iter().filter(|b| !b.choices.is_empty()).count(),
        closes_at,
        ended: ended_at.is_some_and(|at| deadline.is_none_or(|d| at < d)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll(multiple: bool, ends_at: i64) -> Poll {
        Poll {
            options: vec!["Pizza".into(), "Sushi".into(), "Tacos".into()],
            multiple,
            ends_at,
        }
    }

    fn ballot(person: u8, at: i64, sent: u64, choices: &[u8]) -> Ballot {
        Ballot {
            person: [person; 32],
            at,
            sent,
            id: [at as u8; 32],
            choices: choices.to_vec(),
        }
    }

    fn counts(tally: &Tally) -> Vec<usize> {
        tally.voters.iter().map(Vec::len).collect()
    }

    const CREATOR: [u8; 32] = [9; 32];

    #[test]
    fn the_latest_vote_per_person_wins() {
        let ballots = [
            ballot(1, 10, 2_000, &[1]),
            // Older by the sender's clock, though the server took it later.
            ballot(1, 11, 1_000, &[0]),
            ballot(2, 12, 5, &[2]),
            ballot(2, 13, 6, &[]),
            ballot(3, 14, 7, &[2]),
        ];
        let result = tally(&poll(false, 0), &CREATOR, &ballots, &[]);
        assert_eq!(counts(&result), [0, 1, 1]);
        assert_eq!(result.people, 2);
        assert_eq!(result.voters[1], [[1; 32]]);
        assert!(result.open(i64::MAX) && !result.ended);
    }

    #[test]
    fn single_answer_polls_ignore_several_choices() {
        let ballots = [ballot(1, 10, 1, &[0]), ballot(1, 11, 2, &[0, 1])];
        assert_eq!(
            counts(&tally(&poll(false, 0), &CREATOR, &ballots, &[])),
            [1, 0, 0]
        );
        assert_eq!(
            counts(&tally(&poll(true, 0), &CREATOR, &ballots, &[])),
            [1, 1, 0]
        );
    }

    #[test]
    fn malformed_votes_are_ignored() {
        // Option 3 doesn't exist in a three-option poll; the earlier vote stands.
        let ballots = [ballot(1, 10, 1, &[2]), ballot(1, 11, 2, &[3])];
        let result = tally(&poll(true, 0), &CREATOR, &ballots, &[]);
        assert_eq!(counts(&result), [0, 0, 1]);
    }

    #[test]
    fn votes_after_the_end_are_ignored() {
        let ballots = [ballot(1, 10, 1, &[0]), ballot(2, 21, 2, &[1])];
        let timed = tally(&poll(false, 20), &CREATOR, &ballots, &[]);
        assert_eq!(counts(&timed), [1, 0, 0]);
        assert!(timed.open(19) && !timed.open(20) && !timed.ended);
        let ended = tally(&poll(false, 0), &CREATOR, &ballots, &[(CREATOR, 15)]);
        assert_eq!(counts(&ended), [1, 0, 0]);
        assert!(ended.ended && !ended.open(0) && ended.closes_at == Some(15));
    }

    #[test]
    fn only_the_creator_can_end_a_poll() {
        let ballots = [ballot(1, 30, 1, &[0])];
        let result = tally(&poll(false, 0), &CREATOR, &ballots, &[([1; 32], 15)]);
        assert_eq!(counts(&result), [1, 0, 0]);
        assert!(result.open(100) && result.closes_at.is_none());
    }

    #[test]
    fn parts_round_trip_and_reject_bad_framing() {
        let parts = [
            PollPart::Open(poll(true, 1_700_000_000)),
            PollPart::Vote {
                poll: [1; 32],
                sent: 42,
                choices: vec![0, 9],
            },
            PollPart::Vote {
                poll: [1; 32],
                sent: 43,
                choices: Vec::new(),
            },
            PollPart::End { poll: [2; 32] },
        ];
        for part in parts {
            let mut bytes = Vec::new();
            part.encode(&mut bytes).unwrap();
            let mut tail = bytes.as_slice();
            assert_eq!(PollPart::decode(&mut tail).unwrap(), part);
            assert!(tail.is_empty());
            // Every cut is rejected.
            for cut in 0..bytes.len() {
                let mut short = &bytes[..cut];
                assert!(PollPart::decode(&mut short).is_err(), "{cut}");
            }
        }
        let vote = |choices: &[u8]| {
            [
                &[VOTE][..],
                &[1; 32],
                &[0; 8],
                &[choices.len() as u8],
                choices,
            ]
            .concat()
        };
        let open = |multiple: u8, count: u8, option: &[u8]| {
            let mut bytes = vec![OPEN, multiple];
            bytes.extend_from_slice(&0i64.to_be_bytes());
            bytes.push(count);
            for _ in 0..count {
                bytes.extend_from_slice(&(option.len() as u16).to_be_bytes());
                bytes.extend_from_slice(option);
            }
            bytes
        };
        for bad in [
            vote(&[1, 1]),
            vote(&[2, 1]),
            vote(&[10]),
            open(2, 2, b"a"),
            open(0, 1, b"a"),
            open(0, 11, b"a"),
            open(0, 2, b" "),
            open(0, 2, &[0xff]),
            open(0, 2, &[b'x'; 101]),
            vec![4],
        ] {
            assert!(PollPart::decode(&mut bad.as_slice()).is_err(), "{bad:?}");
        }
        let unsorted = PollPart::Vote {
            poll: [1; 32],
            sent: 1,
            choices: vec![1, 0],
        };
        assert!(unsorted.encode(&mut Vec::new()).is_err());
    }

    #[test]
    fn polls_need_a_question_and_two_to_ten_options() {
        let options = |n: usize| vec!["x".to_string(); n];
        assert!(valid_poll("Lunch?", &options(2)));
        assert!(valid_poll("Lunch?", &options(10)));
        assert!(!valid_poll("Lunch?", &options(1)));
        assert!(!valid_poll("Lunch?", &options(11)));
        assert!(!valid_poll("  ", &options(2)));
        assert!(!valid_poll("Lunch?", &["x".into(), "".into()]));
        assert!(!valid_poll(&"q".repeat(MAX_POLL_QUESTION + 1), &options(2)));
    }
}
