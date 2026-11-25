//! Emoji reactions. A reaction travels as a poll vote whose "poll" ID is the first half
//! of the target's message ID followed by half a hash of the emoji. Apps that know polls
//! but not reactions file it under a poll that doesn't exist, so it never shows there
//! as a line; apps with reactions find the message and emoji back from the two halves.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

use super::PollPart;

/// Distinct emoji shown under one message; later ones are left out.
pub const MAX_REACTION_EMOJI: usize = 20;

const DOMAIN: &[u8] = b"rusttalk-reaction-v1";

/// What someone reacted with.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ReactionEmoji {
    /// A Unicode emoji sequence.
    Unicode(String),
    /// A server emoji by ID.
    Custom([u8; 32]),
}

impl ReactionEmoji {
    /// The 16 bytes that name this emoji in a reaction. Variation selector 16 is left
    /// out, so the same emoji with and without it is one reaction.
    pub fn key(&self) -> [u8; 16] {
        let mut hash = Sha256::new().chain_update(DOMAIN);
        hash = match self {
            Self::Unicode(sequence) => hash
                .chain_update([0])
                .chain_update(sequence.replace('\u{fe0f}', "")),
            Self::Custom(id) => hash.chain_update([1]).chain_update(id),
        };
        hash.finalize()[..16].try_into().expect("16 bytes")
    }
}

/// The vote that adds (`on`) or takes back one person's reaction to a message.
/// `sent` is the sender's clock in milliseconds; the latest one counts.
pub fn reaction_vote(target: &[u8; 32], emoji: &[u8; 16], sent: u64, on: bool) -> PollPart {
    let mut poll = [0; 32];
    poll[..16].copy_from_slice(&target[..16]);
    poll[16..].copy_from_slice(emoji);
    PollPart::Vote {
        poll,
        sent,
        choices: if on { vec![0] } else { Vec::new() },
    }
}

/// A reaction as received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub person: [u8; 32],
    /// Server time in unix seconds.
    pub at: i64,
    pub sent: u64,
    /// The record's ID.
    pub id: [u8; 32],
    pub emoji: [u8; 16],
    pub on: bool,
}

impl Reaction {
    /// Read a vote as a reaction: the first half of the target's ID and the reaction.
    /// Any vote with no choice or only the first could be one; a real poll's vote reads
    /// as an emoji key nobody knows, which `tally_reactions` leaves out.
    pub fn from_vote(
        part: &PollPart,
        person: [u8; 32],
        at: i64,
        id: [u8; 32],
    ) -> Option<([u8; 16], Self)> {
        let PollPart::Vote {
            poll,
            sent,
            choices,
        } = part
        else {
            return None;
        };
        let on = match choices.as_slice() {
            [] => false,
            [0] => true,
            _ => return None,
        };
        let reaction = Self {
            person,
            at,
            sent: *sent,
            id,
            emoji: poll[16..].try_into().expect("16 bytes"),
            on,
        };
        Some((poll[..16].try_into().expect("16 bytes"), reaction))
    }
}

/// One message's reactions: each emoji (by key) with who reacted, in the order the
/// emoji and people first came. Each person's latest reaction per emoji counts, an
/// emoji `known` doesn't recognise is left out, and so is any after the first
/// `MAX_REACTION_EMOJI`.
pub fn tally_reactions(
    reactions: &[Reaction],
    known: impl Fn(&[u8; 16]) -> bool,
) -> Vec<([u8; 16], Vec<[u8; 32]>)> {
    let mut latest: HashMap<([u8; 32], [u8; 16]), &Reaction> = HashMap::new();
    for reaction in reactions.iter().filter(|r| known(&r.emoji)) {
        let key = (reaction.person, reaction.emoji);
        let newer = |old: &&Reaction| {
            (old.sent, old.at, old.id) < (reaction.sent, reaction.at, reaction.id)
        };
        if latest.get(&key).is_none_or(newer) {
            latest.insert(key, reaction);
        }
    }
    let mut counted: Vec<&Reaction> = latest.into_values().filter(|r| r.on).collect();
    counted.sort_by_key(|r| (r.at, r.id));
    let mut out: Vec<([u8; 16], Vec<[u8; 32]>)> = Vec::new();
    for reaction in counted {
        if let Some((_, people)) = out.iter_mut().find(|(emoji, _)| *emoji == reaction.emoji) {
            people.push(reaction.person);
        } else if out.len() < MAX_REACTION_EMOJI {
            out.push((reaction.emoji, vec![reaction.person]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::MessageBody;

    const TARGET: [u8; 32] = [7; 32];

    fn heart() -> [u8; 16] {
        ReactionEmoji::Unicode("❤️".into()).key()
    }

    fn react(person: u8, at: i64, sent: u64, emoji: [u8; 16], on: bool) -> Reaction {
        let part = reaction_vote(&TARGET, &emoji, sent, on);
        let (target, reaction) =
            Reaction::from_vote(&part, [person; 32], at, [at as u8; 32]).expect("a reaction");
        assert_eq!(target, TARGET[..16]);
        reaction
    }

    fn any(_: &[u8; 16]) -> bool {
        true
    }

    #[test]
    fn reactions_round_trip_inside_a_vote() {
        let custom = ReactionEmoji::Custom([3; 32]).key();
        for (emoji, on) in [(heart(), true), (custom, false)] {
            let body = MessageBody {
                poll: Some(reaction_vote(&TARGET, &emoji, 42, on)),
                ..Default::default()
            };
            let opened = MessageBody::from_cache(&body.cache_bytes().unwrap()).unwrap();
            let part = opened.poll.expect("a vote");
            let (target, reaction) = Reaction::from_vote(&part, [1; 32], 5, [2; 32]).unwrap();
            assert_eq!(target, TARGET[..16]);
            assert_eq!(
                (reaction.emoji, reaction.on, reaction.sent),
                (emoji, on, 42)
            );
        }
        // The same emoji with or without variation selector 16, and never another.
        assert_eq!(heart(), ReactionEmoji::Unicode("❤".into()).key());
        assert_ne!(heart(), ReactionEmoji::Unicode("👍".into()).key());
        assert_ne!(custom, ReactionEmoji::Custom([4; 32]).key());
        // A vote with any other choices isn't a reaction.
        let vote = PollPart::Vote {
            poll: [1; 32],
            sent: 1,
            choices: vec![1],
        };
        assert!(Reaction::from_vote(&vote, [1; 32], 1, [1; 32]).is_none());
    }

    #[test]
    fn the_latest_reaction_per_person_and_emoji_wins() {
        let thumbs = ReactionEmoji::Unicode("👍".into()).key();
        let reactions = [
            react(1, 10, 2_000, heart(), true),
            // Older by the sender's clock, though the server took it later.
            react(1, 11, 1_000, heart(), false),
            react(2, 12, 5, heart(), true),
            react(2, 13, 6, heart(), false),
            react(3, 14, 7, thumbs, true),
            react(1, 15, 8, thumbs, true),
        ];
        assert_eq!(
            tally_reactions(&reactions, any),
            [(heart(), vec![[1; 32]]), (thumbs, vec![[3; 32], [1; 32]])]
        );
    }

    #[test]
    fn duplicates_count_once_and_unknown_emoji_are_left_out() {
        let reactions = [
            react(1, 10, 1, heart(), true),
            react(1, 10, 1, heart(), true),
            react(2, 11, 1, [9; 16], true),
        ];
        let known = |emoji: &[u8; 16]| *emoji == heart();
        assert_eq!(
            tally_reactions(&reactions, known),
            [(heart(), vec![[1; 32]])]
        );
        // A real poll's vote reads as a key nobody knows.
        let vote = PollPart::Vote {
            poll: [5; 32],
            sent: 1,
            choices: vec![0],
        };
        let (_, reaction) = Reaction::from_vote(&vote, [1; 32], 1, [1; 32]).unwrap();
        assert!(tally_reactions(&[reaction], known).is_empty());
    }

    #[test]
    fn at_most_twenty_distinct_emoji_count() {
        let reactions: Vec<_> = (0..25u8)
            .map(|n| react(1, n as i64, 1, [n; 16], true))
            .collect();
        let tally = tally_reactions(&reactions, any);
        assert_eq!(tally.len(), MAX_REACTION_EMOJI);
        assert_eq!(tally[0].0, [0; 16]);
        assert_eq!(tally[19].0, [19; 16]);
    }
}
