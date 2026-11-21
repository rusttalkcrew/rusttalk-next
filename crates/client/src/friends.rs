//! Friends (#71): who this person is friends with, has asked, was asked by or blocked.
//!
//! The list lives only in the encrypted device-sync blob (`contacts`), so servers never
//! learn it. Requests and answers travel as friend messages in the DM between the two
//! people, which only they can read.

use std::collections::BTreeMap;

use rusttalk_core::client::FriendSignal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{device_sync::Edit, server::Id};

/// Friend messages remembered per person, against copies and reloads.
const SEEN: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    #[default]
    None,
    /// This person asked them.
    Outgoing,
    /// They asked this person.
    Incoming,
    Friend,
    Blocked,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Contact {
    pub relation: Relation,
    /// The last name seen, for the list while no shared server is connected.
    #[serde(default)]
    pub name: String,
    /// When the relation last changed on any device (Unix ms), so the later change wins
    /// when devices sync.
    pub changed: i64,
    /// Server time of the newest friend message handled from them, so an older one
    /// loaded later doesn't undo it.
    #[serde(default)]
    pub since: i64,
    /// Their friend messages handled last (hex IDs), so a copy on another server or a
    /// reload doesn't apply again.
    #[serde(default)]
    pub seen: Vec<String>,
}

/// Something this person does about someone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Add,
    Accept,
    Decline,
    Cancel,
    Remove,
    Block,
    Unblock,
}

#[derive(Default)]
pub struct Friends {
    contacts: BTreeMap<Id, Contact>,
    /// Contacts changed here and not yet published to the other devices.
    dirty: Vec<Id>,
}

impl Friends {
    pub fn relation(&self, person: &Id) -> Relation {
        self.contacts
            .get(person)
            .map_or(Relation::None, |contact| contact.relation)
    }

    pub fn contacts(&self) -> impl Iterator<Item = (&Id, &Contact)> {
        self.contacts.iter()
    }

    /// Do `act`; returns the friend message to send them, if any. `now` is Unix ms.
    pub fn act(&mut self, person: Id, act: Act, name: &str, now: i64) -> Option<FriendSignal> {
        use {Act as A, FriendSignal as S, Relation as R};
        let (relation, send) = match (act, self.relation(&person)) {
            (A::Add, R::None) => (R::Outgoing, Some(S::Request)),
            // Ask again, in case the first one never arrived.
            (A::Add, R::Outgoing) => (R::Outgoing, Some(S::Request)),
            (A::Add | A::Accept, R::Incoming) => (R::Friend, Some(S::Accept)),
            (A::Decline, R::Incoming) => (R::None, Some(S::Decline)),
            (A::Cancel, R::Outgoing) => (R::None, Some(S::Cancel)),
            (A::Remove, R::Friend) => (R::None, Some(S::Remove)),
            // Blocking looks like a removal to them.
            (A::Block, R::Friend) => (R::Blocked, Some(S::Remove)),
            (A::Block, R::Outgoing) => (R::Blocked, Some(S::Cancel)),
            (A::Block, R::Incoming) => (R::Blocked, Some(S::Decline)),
            (A::Block, R::None) => (R::Blocked, None),
            (A::Unblock, R::Blocked) => (R::None, None),
            _ => return None,
        };
        self.set(person, relation, name, now);
        send
    }

    /// Handle a friend message from them: message ID, server time and what it says.
    /// Returns an answer to send, if any. `now` is Unix ms.
    pub fn receive(
        &mut self,
        person: Id,
        (id, at, signal): (Id, i64, FriendSignal),
        name: &str,
        now: i64,
    ) -> Option<FriendSignal> {
        use {FriendSignal as S, Relation as R};
        let id = hex::encode(id);
        let contact = self.contacts.entry(person).or_default();
        if contact.relation == R::Blocked || at < contact.since || contact.seen.contains(&id) {
            return None;
        }
        contact.since = at;
        contact.seen.push(id);
        let extra = contact.seen.len().saturating_sub(SEEN);
        contact.seen.drain(..extra);
        let (relation, reply) = match (signal, contact.relation) {
            (S::Request, R::None) => (R::Incoming, None),
            // Both asked: that's a yes from each.
            (S::Request, R::Outgoing) | (S::Accept, R::Outgoing) => (R::Friend, None),
            // They lost the friendship (say, a removal never reached us): say yes again.
            (S::Request, R::Friend) => (R::Friend, Some(S::Accept)),
            (S::Decline, R::Outgoing) | (S::Cancel, R::Incoming) | (S::Remove, R::Friend) => {
                (R::None, None)
            }
            (_, current) => (current, None),
        };
        if relation != contact.relation {
            self.set(person, relation, name, now);
        }
        // Also when only the seen list changed, so other devices and restarts don't
        // answer this message again. The change time stays, so it wins no conflicts.
        self.mark(person);
        reply
    }

    fn set(&mut self, person: Id, relation: Relation, name: &str, now: i64) {
        let contact = self.contacts.entry(person).or_default();
        contact.relation = relation;
        // Never go back in time, even if this device's clock is behind another's.
        contact.changed = now.max(contact.changed + 1);
        if !name.is_empty() {
            contact.name = name.to_owned();
        }
        self.mark(person);
    }

    fn mark(&mut self, person: Id) {
        if !self.dirty.contains(&person) {
            self.dirty.push(person);
        }
    }

    /// The changed contacts, for the other devices.
    pub fn edits(&mut self) -> Vec<Edit> {
        let dirty = std::mem::take(&mut self.dirty);
        (dirty.into_iter())
            .filter_map(|person| {
                let contact = self.contacts.get(&person)?;
                Some(Edit::Contact(key(&person), to_value(contact)))
            })
            .collect()
    }

    /// Take what the other devices have, keeping the later change per person.
    pub fn adopt(&mut self, synced: &BTreeMap<String, Value>) {
        for (field, value) in synced {
            let (Some(person), Ok(theirs)) = (
                person(field),
                serde_json::from_value::<Contact>(value.clone()),
            ) else {
                tracing::warn!("a synced contact is unreadable; skipped");
                continue;
            };
            let mine = self.contacts.entry(person).or_default();
            let mut seen = std::mem::take(&mut mine.seen);
            for id in &theirs.seen {
                if !seen.contains(id) {
                    seen.push(id.clone());
                }
            }
            let since = mine.since.max(theirs.since);
            if theirs.changed > mine.changed {
                *mine = theirs;
            }
            let extra = seen.len().saturating_sub(SEEN);
            mine.seen = seen.split_off(extra);
            mine.since = since;
        }
    }

    /// Whether a DM from them belongs under "Message requests": the person allows DMs
    /// from friends only, and they aren't one (or asked by this person).
    pub fn is_request(&self, person: &Id, friends_only: bool) -> bool {
        friends_only && !matches!(self.relation(person), Relation::Friend | Relation::Outgoing)
    }

    /// People with a DM, in order, split into the normal list and message requests.
    /// Blocked people are in neither.
    pub fn split(&self, peers: &[Id], friends_only: bool) -> (Vec<Id>, Vec<Id>) {
        let shown = peers
            .iter()
            .filter(|peer| self.relation(peer) != Relation::Blocked);
        shown.partition(|peer| !self.is_request(peer, friends_only))
    }
}

/// Unix time in ms, for `changed`.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn key(person: &Id) -> String {
    hex::encode(person)
}

fn person(key: &str) -> Option<Id> {
    hex::decode(key).ok()?.try_into().ok()
}

fn to_value(contact: &Contact) -> Value {
    serde_json::to_value(contact).expect("contacts serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::friends::{Act as A, Relation as R};
    use rusttalk_core::client::FriendSignal as S;

    const ANA: Id = [1; 32];

    fn msg(id: u8, at: i64, signal: S) -> (Id, i64, S) {
        ([id; 32], at, signal)
    }

    #[test]
    fn request_accept_and_remove() {
        let (mut me, mut ana) = (Friends::default(), Friends::default());
        let bo = [2; 32];
        assert_eq!(me.act(ANA, A::Add, "Ana", 1), Some(S::Request));
        assert_eq!(me.relation(&ANA), R::Outgoing);
        // Asking again resends; accepting a request that isn't there does nothing.
        assert_eq!(me.act(ANA, A::Add, "Ana", 2), Some(S::Request));
        assert_eq!(me.act(ANA, A::Accept, "Ana", 3), None);
        assert_eq!(ana.receive(bo, msg(1, 100, S::Request), "Bo", 4), None);
        assert_eq!(ana.relation(&bo), R::Incoming);
        // The same request again (another server's copy) and the resend change nothing.
        assert_eq!(ana.receive(bo, msg(1, 100, S::Request), "Bo", 5), None);
        assert_eq!(ana.receive(bo, msg(2, 101, S::Request), "Bo", 5), None);
        assert_eq!(ana.act(bo, A::Accept, "Bo", 6), Some(S::Accept));
        assert_eq!(ana.act(bo, A::Accept, "Bo", 7), None);
        assert_eq!(ana.relation(&bo), R::Friend);
        assert_eq!(me.receive(ANA, msg(3, 110, S::Accept), "Ana", 8), None);
        assert_eq!(me.relation(&ANA), R::Friend);
        assert_eq!(me.receive(ANA, msg(3, 110, S::Accept), "Ana", 8), None);
        assert_eq!(me.contacts[&ANA].name, "Ana");
        // Removing tells them; a replay of their old request doesn't bring it back.
        assert_eq!(me.act(ANA, A::Remove, "", 9), Some(S::Remove));
        assert_eq!(me.relation(&ANA), R::None);
        assert_eq!(me.receive(ANA, msg(3, 110, S::Accept), "Ana", 10), None);
        assert_eq!(me.relation(&ANA), R::None);
        assert_eq!(ana.receive(bo, msg(4, 120, S::Remove), "Bo", 11), None);
        assert_eq!(ana.relation(&bo), R::None);
    }

    #[test]
    fn decline_cancel_and_crossed_requests() {
        let mut me = Friends::default();
        me.receive(ANA, msg(1, 100, S::Request), "Ana", 1);
        assert_eq!(me.act(ANA, A::Decline, "Ana", 2), Some(S::Decline));
        assert_eq!(me.relation(&ANA), R::None);
        // The declined request loaded again from history stays declined.
        me.receive(ANA, msg(1, 100, S::Request), "Ana", 3);
        assert_eq!(me.relation(&ANA), R::None);
        // A new request is a new question.
        me.receive(ANA, msg(2, 200, S::Request), "Ana", 4);
        assert_eq!(me.relation(&ANA), R::Incoming);
        me.receive(ANA, msg(3, 210, S::Cancel), "Ana", 5);
        assert_eq!(me.relation(&ANA), R::None);
        // Pages load newest first: an older request after the cancel stays undone.
        me.receive(ANA, msg(4, 205, S::Request), "Ana", 6);
        assert_eq!(me.relation(&ANA), R::None);

        assert_eq!(me.act(ANA, A::Add, "Ana", 7), Some(S::Request));
        assert_eq!(me.act(ANA, A::Cancel, "Ana", 8), Some(S::Cancel));
        assert_eq!(me.act(ANA, A::Cancel, "Ana", 9), None);
        me.act(ANA, A::Add, "Ana", 10);
        me.receive(ANA, msg(5, 300, S::Decline), "Ana", 11);
        assert_eq!(me.relation(&ANA), R::None);
        // Both ask at once: friends without another answer.
        me.act(ANA, A::Add, "Ana", 12);
        assert_eq!(me.receive(ANA, msg(6, 400, S::Request), "Ana", 13), None);
        assert_eq!(me.relation(&ANA), R::Friend);
        // They forgot: answer their new request with a yes.
        assert_eq!(
            me.receive(ANA, msg(7, 500, S::Request), "Ana", 14),
            Some(S::Accept)
        );
    }

    #[test]
    fn blocking_hides_them_and_ignores_their_requests() {
        let mut me = Friends::default();
        me.receive(ANA, msg(1, 100, S::Request), "Ana", 1);
        assert_eq!(me.act(ANA, A::Block, "Ana", 2), Some(S::Decline));
        assert_eq!(me.act(ANA, A::Block, "Ana", 3), None);
        me.receive(ANA, msg(2, 200, S::Request), "Ana", 4);
        assert_eq!(me.relation(&ANA), R::Blocked);
        assert_eq!(me.act(ANA, A::Add, "Ana", 5), None);
        let bo = [2; 32];
        me.act(bo, A::Add, "Bo", 6);
        assert_eq!(me.act(bo, A::Block, "Bo", 7), Some(S::Cancel));
        assert_eq!(me.act(ANA, A::Unblock, "Ana", 8), None);
        assert_eq!(me.relation(&ANA), R::None);
        // Blocked people don't show at all.
        let cy = [3; 32];
        assert_eq!(me.split(&[ANA, bo, cy], false), (vec![ANA, cy], vec![]));
    }

    #[test]
    fn friends_only_moves_other_dms_to_requests() {
        let mut me = Friends::default();
        let (bo, cy, di) = ([2; 32], [3; 32], [4; 32]);
        me.receive(ANA, msg(1, 100, S::Request), "Ana", 1);
        me.act(ANA, A::Accept, "Ana", 2);
        me.act(bo, A::Add, "Bo", 3);
        me.act(di, A::Block, "Di", 4);
        let peers = [cy, ANA, bo, di];
        assert_eq!(me.split(&peers, false), (vec![cy, ANA, bo], vec![]));
        assert_eq!(me.split(&peers, true), (vec![ANA, bo], vec![cy]));
        assert!(me.is_request(&cy, true) && !me.is_request(&cy, false));
        assert!(!me.is_request(&ANA, true));
    }

    #[test]
    fn devices_merge_their_lists_and_the_later_change_wins() {
        let (mut laptop, mut desk) = (Friends::default(), Friends::default());
        let bo = [2; 32];
        laptop.act(ANA, A::Add, "Ana", 1_000);
        laptop.receive(bo, msg(1, 100, S::Request), "Bo", 1_001);
        let published = synced(&mut laptop);
        desk.adopt(&published);
        assert_eq!(desk.relation(&ANA), R::Outgoing);
        assert_eq!(desk.relation(&bo), R::Incoming);
        // Both change Bo while apart: the desk accepts, the laptop blocks a bit later.
        desk.act(bo, A::Accept, "Bo", 2_000);
        laptop.act(bo, A::Block, "Bo", 2_500);
        let (from_desk, from_laptop) = (synced(&mut desk), synced(&mut laptop));
        desk.adopt(&from_laptop);
        laptop.adopt(&from_desk);
        assert_eq!(desk.relation(&bo), R::Blocked);
        assert_eq!(laptop.relation(&bo), R::Blocked);
        // Handled messages carry over, so a new device doesn't answer Bo's old request.
        let mut fresh = Friends::default();
        fresh.adopt(&from_desk);
        assert_eq!(fresh.relation(&bo), R::Friend);
        assert_eq!(
            fresh.receive(bo, msg(1, 100, S::Request), "Bo", 3_000),
            None
        );
    }

    /// The contacts as another device receives them.
    fn synced(friends: &mut Friends) -> BTreeMap<String, Value> {
        let mut state = rusttalk_core::client::sync::State::default();
        for edit in friends.edits() {
            edit.apply(&mut state);
        }
        state.contacts
    }
}
