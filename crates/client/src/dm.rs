//! One DM per person, over every server both people share.
//!
//! Each shared server holds its own copy (a replica): an ordinary DM channel there.
//! A message goes to every replica that is online with the same random ID, so the
//! copies merge here by person and ID.

use std::collections::{HashMap, HashSet};

use url::Url;

use crate::server::{Chat, Id, Msg};

#[derive(Default)]
pub struct Thread {
    pub chat: Chat,
    /// Paging per replica: server and DM channel there.
    pages: HashMap<(Url, Id), Page>,
    /// Person and ID of messages deleted on some server this session, so a copy on
    /// another server doesn't bring them back.
    gone: HashSet<(Id, Id)>,
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

    /// Add a page or live messages from one replica. Returns the IDs of `me`'s
    /// messages that were deleted elsewhere but are still on this one.
    pub fn add(
        &mut self,
        server: &Url,
        channel: Id,
        mut items: Vec<Msg>,
        older: bool,
        at_start: Option<bool>,
        me: &Id,
    ) -> Vec<Id> {
        // A sent message can arrive before its replica is opened; opening still has to happen.
        if let Some(page) = self.pages.get_mut(&(server.clone(), channel))
            && let Some(at_start) = at_start
        {
            page.loading = false;
            page.at_start |= at_start;
            if (older || page.oldest.is_none())
                && let Some(first) = items.first()
            {
                page.oldest = Some(first.id);
            }
        }
        let mut stale = Vec::new();
        items.retain(|msg| {
            let gone = self.gone.contains(&(msg.person, msg.id));
            if gone && msg.person == *me {
                stale.push(msg.id);
            }
            !gone
        });
        for msg in &mut items {
            msg.replicas = vec![server.clone()];
        }
        self.chat.merge(items, older);
        self.refresh();
        stale
    }

    /// A replica deleted a message: remove it here, whichever servers hold it.
    pub fn remove(&mut self, server: &Url, id: &Id) {
        let Some(person) = (self.chat.messages.iter())
            .find(|msg| msg.id == *id && msg.replicas.contains(server))
            .map(|msg| msg.person)
        else {
            return;
        };
        self.gone.insert((person, *id));
        self.chat
            .messages
            .retain(|msg| msg.id != *id || msg.person != person);
        if !self.chat.messages.iter().any(|msg| msg.id == *id) {
            self.chat.remove(id);
        }
    }

    /// Older pages to ask for, on servers that are online: server, channel and cursor.
    pub fn older(&mut self, online: impl Fn(&Url) -> bool) -> Vec<(Url, Id, Id)> {
        let mut asks = Vec::new();
        for ((server, channel), page) in &mut self.pages {
            if let Some(oldest) = page.oldest
                && !page.at_start
                && !page.loading
                && online(server)
            {
                page.loading = true;
                asks.push((server.clone(), *channel, oldest));
            }
        }
        self.refresh();
        asks
    }

    /// The server is gone: stop paging there. Its messages stay readable here.
    pub fn forget(&mut self, server: &Url) {
        self.pages.retain(|(at, _), _| at != server);
        self.refresh();
    }

    /// Whether this is the first copy of a live message to arrive.
    pub fn first_arrival(&mut self, person: Id, id: Id) -> bool {
        self.arrived.insert((person, id))
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
    use crate::server::Body;

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
        let me = [7; 32];
        thread.add(
            &a,
            [1; 32],
            vec![msg(1, 7, 10, "1"), msg(2, 8, 20, "2"), msg(3, 7, 30, "3")],
            false,
            Some(true),
            &me,
        );
        assert!(thread.chat.loading);
        thread.add(
            &b,
            [2; 32],
            vec![msg(1, 7, 12, "1"), msg(3, 7, 31, "3")],
            false,
            Some(true),
            &me,
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
        thread.add(
            &b,
            [2; 32],
            vec![msg(3, 8, 32, "theirs"), edit],
            false,
            None,
            &me,
        );
        assert_eq!(texts(&thread), ["1", "2", "3", "forged"]);
    }

    #[test]
    fn a_readable_copy_replaces_an_unreadable_one() {
        let (a, b) = (server(1), server(2));
        let me = [7; 32];
        let mut thread = Thread::default();
        let unreadable = Msg {
            body: Body::Unreadable,
            ..msg(1, 8, 10, "")
        };
        thread.add(&b, [2; 32], vec![unreadable], false, None, &me);
        thread.add(&a, [1; 32], vec![msg(1, 8, 11, "hi")], false, None, &me);
        assert_eq!(texts(&thread), ["hi"]);
        // The shown copy's server comes first, so its files download from there.
        assert_eq!(thread.chat.messages[0].replicas, [a, b]);
        assert_eq!(thread.chat.messages[0].at, 10);
    }

    #[test]
    fn deletes_apply_everywhere_and_stale_copies_are_reported() {
        let (a, b) = (server(1), server(2));
        let me = [7; 32];
        let mut thread = Thread::default();
        thread.add(&a, [1; 32], vec![msg(1, 7, 10, "mine")], false, None, &me);
        thread.add(&a, [1; 32], vec![msg(2, 8, 11, "theirs")], false, None, &me);
        // A delete on a server that doesn't hold the message changes nothing.
        thread.remove(&b, &[1; 32]);
        assert_eq!(texts(&thread), ["mine", "theirs"]);
        thread.remove(&a, &[1; 32]);
        thread.remove(&a, &[2; 32]);
        assert!(thread.chat.messages.is_empty());
        // B was offline during the deletes: its copies stay hidden, and mine is
        // reported so it can be deleted there too.
        let stale = thread.add(
            &b,
            [2; 32],
            vec![msg(1, 7, 10, "mine"), msg(2, 8, 11, "theirs")],
            false,
            Some(true),
            &me,
        );
        assert_eq!(stale, [[1; 32]]);
        assert!(thread.chat.messages.is_empty());
    }

    #[test]
    fn pages_each_replica_from_its_own_oldest_message() {
        let (a, b) = (server(1), server(2));
        let me = [7; 32];
        let mut thread = Thread::default();
        thread.open(&a, [1; 32]);
        thread.open(&b, [2; 32]);
        thread.add(
            &a,
            [1; 32],
            vec![msg(5, 7, 50, "5")],
            false,
            Some(false),
            &me,
        );
        thread.add(
            &b,
            [2; 32],
            vec![msg(6, 7, 60, "6")],
            false,
            Some(true),
            &me,
        );
        assert!(!thread.chat.at_start);
        // Only online servers, and only replicas with more to load.
        assert!(thread.older(|_| false).is_empty());
        assert_eq!(thread.older(|_| true), [(a.clone(), [1; 32], [5; 32])]);
        assert!(thread.older(|_| true).is_empty() && thread.chat.loading);
        thread.add(&a, [1; 32], vec![msg(4, 7, 40, "4")], true, Some(true), &me);
        assert!(thread.chat.at_start && !thread.chat.loading);
        assert_eq!(texts(&thread), ["4", "5", "6"]);
        // A live message doesn't move the paging cursor.
        thread.add(&a, [1; 32], vec![msg(7, 7, 70, "7")], false, None, &me);
        // One sent on a replica not opened yet leaves it to be opened.
        thread.add(&a, [3; 32], vec![msg(8, 7, 80, "8")], false, None, &me);
        assert!(thread.open(&a, [3; 32]));
        thread.add(
            &a,
            [3; 32],
            vec![msg(8, 7, 80, "8")],
            false,
            Some(true),
            &me,
        );
        thread.forget(&a);
        assert!(thread.chat.at_start && thread.unanswered(&a).is_empty());
        assert_eq!(texts(&thread), ["4", "5", "6", "7", "8"]);
    }
}
