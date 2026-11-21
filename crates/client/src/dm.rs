//! One DM per person, over every server both people share.
//!
//! Each shared server holds its own copy (a replica): an ordinary DM channel there.
//! A message goes to every replica that is online with the same random ID, so the
//! copies merge here by person and ID.

use std::collections::{BTreeSet, HashMap, HashSet};

use url::Url;

use crate::server::{Body, Chat, Id, Msg};

#[derive(Default)]
pub struct Thread {
    pub chat: Chat,
    /// Paging per replica: server and DM channel there.
    pages: HashMap<(Url, Id), Page>,
    /// Person and ID of messages that arrived live, so each notifies once.
    // shortcut: kept for the session; DMs are small enough.
    arrived: HashSet<(Id, Id)>,
}

#[derive(Default)]
struct Page {
    /// The oldest message loaded from this replica, to page from.
    oldest: Option<Id>,
    at_start: bool,
    loading: bool,
    /// The last page failed; asked again once the server reconnects.
    failed: bool,
}

/// Who a DM is between, and this person's own messages they deleted on any device.
pub struct Pair<'a> {
    pub me: Id,
    pub peer: Id,
    pub deleted: &'a Tombstones,
}

/// Resolved deletes kept for copies that show up on a server not known to hold one.
const KEEP_RESOLVED: usize = 1_000;
/// Unresolved deletes are never dropped below this; it keeps the sync copy in its size limit.
// shortcut: past this many deletes still waiting for an offline server, the oldest go.
const MAX_UNRESOLVED: usize = 2_000;

/// This person's deleted DM messages, oldest first, synced between their devices.
/// Each merges as a set, so two devices deleting on different servers keep both.
#[derive(Clone, Default, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tombstones(Vec<Tombstone>);

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Tombstone {
    #[serde(with = "base64_id")]
    id: Id,
    /// Servers that may still hold a copy.
    left: BTreeSet<Url>,
    /// Servers whose copy is gone.
    done: BTreeSet<Url>,
}

impl Tombstones {
    pub fn contains(&self, id: &Id) -> bool {
        self.0.iter().any(|stone| stone.id == *id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &Id> {
        self.0.iter().map(|stone| &stone.id)
    }

    /// Record a delete, or more servers that may hold a copy. `false` when nothing changed.
    pub fn add(&mut self, id: Id, servers: impl IntoIterator<Item = Url>) -> bool {
        let before = self.clone();
        let at = match self.0.iter().position(|stone| stone.id == id) {
            Some(at) => at,
            None => {
                self.0.push(Tombstone {
                    id,
                    left: BTreeSet::new(),
                    done: BTreeSet::new(),
                });
                self.0.len() - 1
            }
        };
        let stone = &mut self.0[at];
        stone.left.extend(servers);
        stone.left.retain(|server| !stone.done.contains(server));
        self.compact();
        *self != before
    }

    /// A server's copy is gone. `false` when that changes nothing or isn't a delete of mine.
    pub fn gone(&mut self, id: &Id, server: &Url) -> bool {
        let Some(stone) = self.0.iter_mut().find(|stone| stone.id == *id) else {
            return false;
        };
        stone.left.remove(server);
        let changed = stone.done.insert(server.clone());
        self.compact();
        changed
    }

    /// Add another device's or snapshot's deletes: every ID, and what each knows
    /// about the servers; a copy gone anywhere is gone.
    pub fn merge(&mut self, other: &Self) {
        for theirs in &other.0 {
            match self.0.iter_mut().find(|stone| stone.id == theirs.id) {
                Some(mine) => {
                    mine.done.extend(theirs.done.iter().cloned());
                    mine.left.extend(theirs.left.iter().cloned());
                    let done = &mine.done;
                    mine.left.retain(|server| !done.contains(server));
                }
                None => self.0.push(theirs.clone()),
            }
        }
        self.compact();
    }

    /// Drop the oldest deletes that no known server still holds, past `KEEP_RESOLVED`.
    fn compact(&mut self) {
        let resolved = self.0.iter().filter(|stone| stone.left.is_empty()).count();
        let mut extra = resolved.saturating_sub(KEEP_RESOLVED);
        self.0.retain(|stone| {
            let drop = extra > 0 && stone.left.is_empty();
            extra -= usize::from(drop);
            !drop
        });
        let extra = self.0.len().saturating_sub(KEEP_RESOLVED + MAX_UNRESOLVED);
        self.0.drain(..extra);
    }
}

mod base64_id {
    use rusttalk_core::auth::{decode, encode};

    pub fn serialize<S: serde::Serializer>(id: &[u8; 32], out: S) -> Result<S::Ok, S::Error> {
        out.serialize_str(&encode(id))
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(input: D) -> Result<[u8; 32], D::Error> {
        let text: String = serde::Deserialize::deserialize(input)?;
        decode(&text).map_err(serde::de::Error::custom)
    }
}

impl Thread {
    /// Start loading a replica; `false` when it is already loaded or loading.
    pub fn open(&mut self, server: &Url, channel: Id) -> bool {
        let key = (server.clone(), channel);
        if self.pages.contains_key(&key) {
            return false;
        }
        self.pages.insert(
            key,
            Page {
                loading: true,
                ..Page::default()
            },
        );
        self.refresh();
        true
    }

    /// Replicas whose first page was asked for but never came, on this server.
    pub fn unanswered(&self, server: &Url) -> Vec<Id> {
        (self.pages.iter())
            .filter(|((at, _), page)| at == server && page.loading && page.oldest.is_none())
            .map(|((_, channel), _)| *channel)
            .collect()
    }

    /// Add a page or live messages from one replica. Returns the IDs of my messages
    /// I deleted that are still on this one, to delete there too.
    pub fn add(
        &mut self,
        server: &Url,
        channel: Id,
        mut items: Vec<Msg>,
        older: bool,
        at_start: Option<bool>,
        pair: &Pair,
    ) -> Vec<Id> {
        // A sent message can arrive before its replica is opened; opening still has to happen.
        if let Some(page) = self.pages.get_mut(&(server.clone(), channel))
            && let Some(at_start) = at_start
        {
            page.loading = false;
            page.failed = false;
            page.at_start |= at_start;
            if (older || page.oldest.is_none())
                && let Some(first) = items.first()
            {
                page.oldest = Some(first.id);
            }
        }
        let mut stale = Vec::new();
        items.retain(|msg| {
            // Only the two people can write in a DM; a record from anyone else was
            // put there by the server.
            if msg.person != pair.me && msg.person != pair.peer {
                return false;
            }
            let deleted = msg.person == pair.me && pair.deleted.contains(&msg.id);
            // Only a copy that opened is mine for sure, so only that one is deleted.
            if deleted && msg.body != Body::Unreadable {
                stale.push(msg.id);
            }
            !deleted
        });
        for msg in &mut items {
            msg.replicas = vec![server.clone()];
        }
        self.chat.merge(items, older);
        self.refresh();
        stale
    }

    /// A server deleted a message. That only removes its copy: a server can't prove
    /// who deleted it, and retention deletes old messages on each server separately.
    /// The message goes once no server that showed it holds it any more.
    pub fn remove(&mut self, server: &Url, id: &Id) {
        let mut gone = Vec::new();
        for msg in (self.chat.messages.iter_mut()).filter(|msg| msg.id == *id) {
            if !msg.replicas.contains(server) {
                continue;
            }
            let shown = msg.replicas[0] == *server;
            msg.replicas.retain(|held| held != server);
            msg.copy_files.retain(|(held, _)| held != server);
            match msg.replicas.first() {
                None => gone.push(msg.person),
                // The next copy shows now, with its own files.
                Some(next) if shown => {
                    if let Some(at) = msg.copy_files.iter().position(|(held, _)| held == next) {
                        msg.files = msg.copy_files.remove(at).1;
                    }
                }
                Some(_) => {}
            }
        }
        for person in gone {
            self.chat.remove_one(&person, id);
        }
    }

    /// I deleted my message: hide it whichever servers still hold it.
    pub fn deleted(&mut self, me: &Id, id: &Id) {
        self.chat.remove_one(me, id);
    }

    /// Older pages to ask for, on servers that are online: server, channel and the
    /// cursor, or `None` to load the newest page again.
    pub fn older(&mut self, online: impl Fn(&Url) -> bool) -> Vec<(Url, Id, Option<Id>)> {
        let mut asks = Vec::new();
        for ((server, channel), page) in &mut self.pages {
            if !page.at_start && !page.loading && !page.failed && online(server) {
                page.loading = true;
                asks.push((server.clone(), *channel, page.oldest));
            }
        }
        self.refresh();
        asks
    }

    /// A page failed. It stops blocking the other replicas. When its cursor is gone
    /// from that server (`missing`), paging there starts over from the newest page;
    /// otherwise it waits for the server to reconnect, which loads it again.
    pub fn page_failed(&mut self, server: &Url, channel: Id, missing: bool) {
        if let Some(page) = self.pages.get_mut(&(server.clone(), channel)) {
            page.loading = false;
            if missing {
                page.oldest = None;
            } else {
                page.failed = true;
            }
        }
        self.refresh();
    }

    /// The server is gone: stop paging there. Its messages stay readable here.
    pub fn forget(&mut self, server: &Url) {
        self.pages.retain(|(at, _), _| at != server);
        self.refresh();
    }

    /// Whether this is the first readable copy of a live message to arrive. One
    /// that couldn't be opened doesn't count, so a forged record can't use it up.
    pub fn first_arrival(&mut self, msg: &Msg) -> bool {
        msg.body != Body::Unreadable && self.arrived.insert((msg.person, msg.id))
    }

    /// The whole chat is loading while any replica is, and at its start once all are.
    pub fn refresh(&mut self) {
        self.chat.loading = self.pages.values().any(|page| page.loading);
        self.chat.at_start = self.pages.values().all(|page| page.at_start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusttalk_core::client::{Attachment, BlobRef};

    const ME: Id = [7; 32];
    const PEER: Id = [8; 32];

    fn server(n: u8) -> Url {
        format!("https://127.0.0.1:{}/", 9000 + u16::from(n))
            .parse()
            .unwrap()
    }

    fn msg(id: u8, person: u8, at: i64, text: &str) -> Msg {
        Msg {
            id: [id; 32],
            person: [person; 32],
            at,
            reply_to: None,
            edit_of: None,
            body: Body::Text(text.into()),
            files: Vec::new(),
            live: None,
            sticker: None,
            replicas: Vec::new(),
            copy_files: Vec::new(),
        }
    }

    fn unreadable(id: u8, person: u8, at: i64) -> Msg {
        Msg {
            body: Body::Unreadable,
            ..msg(id, person, at, "")
        }
    }

    fn pair(deleted: &Tombstones) -> Pair<'_> {
        Pair {
            me: ME,
            peer: PEER,
            deleted,
        }
    }

    fn texts(thread: &Thread) -> Vec<&str> {
        (thread.chat.messages.iter())
            .map(|msg| match &msg.body {
                Body::Text(text) => text.as_str(),
                _ => "?",
            })
            .collect()
    }

    #[test]
    fn merges_replicas_by_person_and_id_in_server_time_order() {
        let (a, b) = (server(1), server(2));
        let mut thread = Thread::default();
        assert!(thread.open(&a, [1; 32]) && thread.open(&b, [2; 32]));
        assert!(!thread.open(&a, [1; 32]));
        // Server B was down for "2" and its clock runs ahead.
        let page = vec![msg(1, 7, 10, "1"), msg(2, 8, 20, "2"), msg(3, 7, 30, "3")];
        thread.add(
            &a,
            [1; 32],
            page,
            false,
            Some(true),
            &pair(&Tombstones::default()),
        );
        assert!(thread.chat.loading);
        let page = vec![msg(1, 7, 12, "1"), msg(3, 7, 31, "3")];
        thread.add(
            &b,
            [2; 32],
            page,
            false,
            Some(true),
            &pair(&Tombstones::default()),
        );
        assert!(!thread.chat.loading && thread.chat.at_start);
        assert_eq!(texts(&thread), ["1", "2", "3"]);
        let first = &thread.chat.messages[0];
        assert_eq!(
            (first.at, first.replicas.clone()),
            (10, vec![a.clone(), b.clone()])
        );
        assert_eq!(thread.chat.messages[1].replicas, std::slice::from_ref(&a));
        // Someone else's record under one of my IDs stays a separate message, and
        // their edit doesn't change mine.
        let edit = Msg {
            edit_of: Some([3; 32]),
            ..msg(4, 8, 40, "forged")
        };
        let items = vec![msg(3, 8, 32, "theirs"), edit];
        thread.add(
            &b,
            [2; 32],
            items,
            false,
            None,
            &pair(&Tombstones::default()),
        );
        assert_eq!(texts(&thread), ["1", "2", "3", "forged"]);
    }

    #[test]
    fn a_readable_copy_replaces_an_unreadable_one() {
        let (a, b) = (server(1), server(2));
        let mut thread = Thread::default();
        thread.add(
            &b,
            [2; 32],
            vec![unreadable(1, 8, 10)],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        thread.add(
            &a,
            [1; 32],
            vec![msg(1, 8, 11, "hi")],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        assert_eq!(texts(&thread), ["hi"]);
        // The placeholder's server and time are gone with it.
        assert_eq!(thread.chat.messages[0].replicas, [a]);
        assert_eq!(thread.chat.messages[0].at, 11);
    }

    #[test]
    fn copies_that_fail_to_open_stay_on_their_own_server() {
        let (a, b) = (server(1), server(2));
        let mut thread = Thread::default();
        let real = msg(1, 8, 50, "hi");
        // A forged record under the same person and ID, from a server that dates it
        // early, arrives first.
        let forged = unreadable(1, 8, 1);
        assert!(!thread.first_arrival(&forged));
        thread.add(
            &b,
            [2; 32],
            vec![forged.clone()],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        // The real copy still notifies once, and keeps its own time and server.
        assert!(thread.first_arrival(&real));
        assert!(!thread.first_arrival(&real));
        thread.add(
            &a,
            [1; 32],
            vec![real],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        thread.add(
            &b,
            [2; 32],
            vec![forged],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        let shown = &thread.chat.messages[0];
        assert_eq!((shown.at, shown.replicas.clone()), (50, vec![a.clone()]));
        // So a delete on B doesn't touch it either.
        thread.remove(&b, &[1; 32]);
        assert_eq!(texts(&thread), ["hi"]);
        // Nobody but the two people writes in a DM.
        thread.add(
            &b,
            [2; 32],
            vec![msg(2, 9, 60, "intruder")],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        assert_eq!(texts(&thread), ["hi"]);
    }

    #[test]
    fn a_server_delete_only_removes_that_servers_copy() {
        let (a, b) = (server(1), server(2));
        let mut thread = Thread::default();
        let both = vec![msg(1, 7, 10, "mine"), msg(2, 8, 11, "theirs")];
        thread.add(
            &a,
            [1; 32],
            both.clone(),
            false,
            None,
            &pair(&Tombstones::default()),
        );
        thread.add(
            &b,
            [2; 32],
            both.clone(),
            false,
            None,
            &pair(&Tombstones::default()),
        );
        // Retention, or a dishonest server, deletes both on B.
        thread.remove(&b, &[1; 32]);
        thread.remove(&b, &[2; 32]);
        assert_eq!(texts(&thread), ["mine", "theirs"]);
        assert_eq!(thread.chat.messages[0].replicas, std::slice::from_ref(&a));
        // A delete on a server that doesn't hold the message changes nothing.
        thread.remove(&server(3), &[1; 32]);
        assert_eq!(texts(&thread), ["mine", "theirs"]);
        // Once A deletes them too, nothing shows them any more.
        thread.remove(&a, &[1; 32]);
        thread.remove(&a, &[2; 32]);
        assert!(thread.chat.messages.is_empty());
        // B's copies come back on reload, and none is deleted there: no server
        // proved that I deleted it.
        let stale = thread.add(
            &b,
            [2; 32],
            both,
            false,
            Some(true),
            &pair(&Tombstones::default()),
        );
        assert!(stale.is_empty());
        assert_eq!(texts(&thread), ["mine", "theirs"]);
    }

    #[test]
    fn my_deletes_hide_every_copy_and_survive_a_restart() {
        let (a, b) = (server(1), server(2));
        let mut deleted = Tombstones::default();
        deleted.add([1; 32], [a.clone()]);
        let mut thread = Thread::default();
        thread.add(
            &a,
            [1; 32],
            vec![msg(1, 7, 10, "mine")],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        thread.deleted(&ME, &[1; 32]);
        assert!(thread.chat.messages.is_empty());
        // After a restart, B was away during the delete: its copy stays hidden and is
        // reported so it gets deleted there. A peer record under that ID is not mine.
        let mut restarted = Thread::default();
        let items = vec![msg(1, 7, 10, "mine"), msg(1, 8, 12, "theirs")];
        let stale = restarted.add(&b, [2; 32], items, false, Some(true), &pair(&deleted));
        assert_eq!(stale, [[1; 32]]);
        assert_eq!(texts(&restarted), ["theirs"]);
    }

    #[test]
    fn only_a_readable_copy_of_mine_is_deleted_again() {
        let b = server(2);
        let mut deleted = Tombstones::default();
        deleted.add([1; 32], []);
        let mut thread = Thread::default();
        // A copy under my deleted ID that doesn't open could be anyone's forgery:
        // it stays hidden, but nothing is deleted for it.
        let stale = thread.add(
            &b,
            [2; 32],
            vec![unreadable(1, 7, 10)],
            false,
            None,
            &pair(&deleted),
        );
        assert!(stale.is_empty() && thread.chat.messages.is_empty());
    }

    #[test]
    fn unresolved_deletes_are_never_dropped_for_newer_ones() {
        let (a, b) = (server(1), server(2));
        let mut deleted = Tombstones::default();
        // B was away for the first delete; A confirmed the rest.
        deleted.add([0; 32], [b.clone()]);
        for n in 1..=KEEP_RESOLVED as u16 + 50 {
            let mut id = [9; 32];
            id[..2].copy_from_slice(&n.to_be_bytes());
            deleted.add(id, [a.clone()]);
            assert!(deleted.gone(&id, &a));
        }
        assert!(deleted.contains(&[0; 32]));
        assert_eq!(deleted.ids().count(), KEEP_RESOLVED + 1);
        // B's copy is gone at last: now it may go too, oldest first.
        assert!(deleted.gone(&[0; 32], &b) && !deleted.gone(&[0; 32], &b));
        assert!(!deleted.contains(&[0; 32]));
    }

    #[test]
    fn files_follow_the_copy_that_is_shown() {
        let (a, b) = (server(1), server(2));
        let file = |id: u8| Attachment {
            file: BlobRef::for_tests([id; 32]),
            name: "a.txt".into(),
            media_type: "text/plain".into(),
            thumbnail: None,
        };
        let copy = |blob: u8| Msg {
            files: vec![file(blob)],
            ..msg(1, 8, 10, "hi")
        };
        let mut thread = Thread::default();
        thread.add(
            &a,
            [1; 32],
            vec![copy(1)],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        thread.add(
            &b,
            [2; 32],
            vec![copy(2)],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        assert_eq!(thread.chat.messages[0].files[0].file.id, [1; 32]);
        // A drops its copy: B's copy shows, with B's own blob.
        thread.remove(&a, &[1; 32]);
        let shown = &thread.chat.messages[0];
        assert_eq!(shown.replicas, std::slice::from_ref(&b));
        assert_eq!(shown.files[0].file.id, [2; 32]);
    }

    #[test]
    fn a_failed_page_doesnt_hold_up_the_other_replicas() {
        let (a, b) = (server(1), server(2));
        let mut thread = Thread::default();
        thread.open(&a, [1; 32]);
        thread.open(&b, [2; 32]);
        thread.add(
            &a,
            [1; 32],
            vec![msg(5, 7, 50, "5")],
            false,
            Some(false),
            &pair(&Tombstones::default()),
        );
        // B's first page fails: the chat isn't loading for good, and A still pages.
        thread.page_failed(&b, [2; 32], false);
        assert!(!thread.chat.loading);
        assert_eq!(
            thread.older(|_| true),
            [(a.clone(), [1; 32], Some([5; 32]))]
        );
        thread.add(
            &a,
            [1; 32],
            vec![msg(4, 7, 40, "4")],
            true,
            Some(false),
            &pair(&Tombstones::default()),
        );
        // B loads again on reconnect.
        thread.add(
            &b,
            [2; 32],
            vec![msg(6, 7, 60, "6")],
            false,
            Some(false),
            &pair(&Tombstones::default()),
        );
        // A's cursor was deleted there meanwhile: A starts over from its newest page,
        // and B pages on.
        let mut asks = thread.older(|_| true);
        asks.sort_by_key(|(server, ..)| server.port());
        assert_eq!(asks[1], (b.clone(), [2; 32], Some([6; 32])));
        // B answers while A is still loading: B pages on without waiting for A.
        let older = vec![msg(3, 7, 30, "3")];
        thread.add(
            &b,
            [2; 32],
            older,
            true,
            Some(false),
            &pair(&Tombstones::default()),
        );
        assert!(thread.chat.loading);
        assert_eq!(
            thread.older(|_| true),
            [(b.clone(), [2; 32], Some([3; 32]))]
        );
        thread.page_failed(&a, [1; 32], true);
        assert!(thread.chat.loading);
        assert_eq!(thread.older(|_| true), [(a.clone(), [1; 32], None)]);
        thread.add(
            &a,
            [1; 32],
            vec![msg(5, 7, 50, "5")],
            false,
            Some(true),
            &pair(&Tombstones::default()),
        );
        assert_eq!(texts(&thread), ["3", "4", "5", "6"]);
    }

    #[test]
    fn pages_each_replica_from_its_own_oldest_message() {
        let (a, b) = (server(1), server(2));
        let mut thread = Thread::default();
        thread.open(&a, [1; 32]);
        thread.open(&b, [2; 32]);
        thread.add(
            &a,
            [1; 32],
            vec![msg(5, 7, 50, "5")],
            false,
            Some(false),
            &pair(&Tombstones::default()),
        );
        thread.add(
            &b,
            [2; 32],
            vec![msg(6, 7, 60, "6")],
            false,
            Some(true),
            &pair(&Tombstones::default()),
        );
        assert!(!thread.chat.at_start);
        // Only online servers, and only replicas with more to load.
        assert!(thread.older(|_| false).is_empty());
        assert_eq!(
            thread.older(|_| true),
            [(a.clone(), [1; 32], Some([5; 32]))]
        );
        assert!(thread.older(|_| true).is_empty() && thread.chat.loading);
        thread.add(
            &a,
            [1; 32],
            vec![msg(4, 7, 40, "4")],
            true,
            Some(true),
            &pair(&Tombstones::default()),
        );
        assert!(thread.chat.at_start && !thread.chat.loading);
        assert_eq!(texts(&thread), ["4", "5", "6"]);
        // A live message doesn't move the paging cursor.
        thread.add(
            &a,
            [1; 32],
            vec![msg(7, 7, 70, "7")],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        // One sent on a replica not opened yet leaves it to be opened.
        thread.add(
            &a,
            [3; 32],
            vec![msg(8, 7, 80, "8")],
            false,
            None,
            &pair(&Tombstones::default()),
        );
        assert!(thread.open(&a, [3; 32]));
        thread.add(
            &a,
            [3; 32],
            vec![msg(8, 7, 80, "8")],
            false,
            Some(true),
            &pair(&Tombstones::default()),
        );
        thread.forget(&a);
        assert!(thread.chat.at_start && thread.unanswered(&a).is_empty());
        assert_eq!(texts(&thread), ["4", "5", "6", "7", "8"]);
    }
}
