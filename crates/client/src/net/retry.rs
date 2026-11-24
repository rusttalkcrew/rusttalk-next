//! Transfers a dropped connection stopped, asked for again once it is back.

use std::{collections::VecDeque, path::PathBuf};

use super::{Command, Fetch};
use crate::{server::Id, sticker_files::Kind};

/// How many connection attempts a stopped transfer is kept through, so one that keeps
/// failing (or a server that stays down, about 25 s of reconnects) ends as "offline".
const RETRIES: u32 = 5;

/// Which transfer a command asks for, to count its tries across connections.
#[derive(PartialEq)]
enum Key {
    Send(Id),
    Image([u8; 32], Fetch),
    LinkCard([u8; 32]),
    StickerFile([u8; 32], Kind),
    Save([u8; 32], PathBuf),
    /// A channel drive share, by the files it still has to upload.
    Share(Id, Vec<PathBuf>),
}

fn key(command: &Command) -> Option<Key> {
    Some(match command {
        Command::Send(draft) => Key::Send(draft.id),
        Command::Image(file, fetch) => Key::Image(file.id, *fetch),
        Command::LinkCard(file) => Key::LinkCard(file.id),
        Command::StickerFile(file, kind) => Key::StickerFile(file.id, *kind),
        Command::Save(file, path) => Key::Save(file.id, path.clone()),
        Command::ShareFiles(channel, paths) => Key::Share(*channel, paths.clone()),
        _ => return None,
    })
}

/// Stopped transfers waiting for the next connection, with how often each was tried.
/// Not in `State` for the same reason as `queued`: commands aren't all `Sync`.
#[derive(Default)]
pub(super) struct Retries {
    pending: VecDeque<Command>,
    tries: Vec<(Key, u32)>,
}

impl Retries {
    /// Put the transfers to retry in front of what was asked since, in order, for the
    /// connection about to be made (`serve` runs `queued` once it is up).
    pub(super) fn requeue(&mut self, queued: &mut VecDeque<Command>) {
        for command in self.pending.drain(..).rev() {
            queued.push_front(command);
        }
    }

    /// A connection attempt ended. `stopped` are the transfers it stopped, in order; the
    /// retries it never got to are taken back out of `queued`. Returns the ones tried too
    /// often, to refuse. A transfer that finished is forgotten.
    pub(super) fn ended(
        &mut self,
        stopped: Vec<Command>,
        queued: &mut VecDeque<Command>,
    ) -> Vec<Command> {
        let waiting = |command: &Command| {
            key(command).is_some_and(|key| self.tries.iter().any(|(tried, _)| *tried == key))
        };
        let (requeued, rest): (VecDeque<_>, VecDeque<_>) = queued.drain(..).partition(waiting);
        *queued = rest;
        let unrun = std::mem::take(&mut self.pending)
            .into_iter()
            .chain(requeued);
        let before = std::mem::take(&mut self.tries);
        let mut refused = Vec::new();
        for command in stopped.into_iter().chain(unrun) {
            let Some(key) = key(&command) else {
                refused.push(command);
                continue;
            };
            let tries = (before.iter())
                .find(|(tried, _)| *tried == key)
                .map_or(0, |(_, tries)| *tries)
                + 1;
            if tries > RETRIES {
                refused.push(command);
                continue;
            }
            if !self.tries.iter().any(|(tried, _)| *tried == key) {
                self.tries.push((key, tries));
            }
            self.pending.push_back(command);
        }
        refused
    }
}

#[cfg(test)]
mod tests {
    use rusttalk_core::client::BlobRef;

    use super::*;

    fn card(id: u8) -> Command {
        Command::LinkCard(BlobRef::for_tests([id; 32]))
    }

    fn ids(commands: impl IntoIterator<Item = Command>) -> Vec<u8> {
        (commands.into_iter())
            .map(|command| match command {
                Command::LinkCard(file) => file.id[0],
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn stopped_transfers_go_first_on_the_next_connection_in_order() {
        let mut retries = Retries::default();
        // Asked while connecting; refused as before when this attempt fails.
        let mut queued = VecDeque::from([card(9)]);
        assert!(
            retries
                .ended(vec![card(1), card(2)], &mut queued)
                .is_empty()
        );
        assert_eq!(ids(queued.drain(..)), [9]);
        queued.push_back(card(8));
        retries.requeue(&mut queued);
        assert_eq!(ids(queued.drain(..)), [1, 2, 8]);
    }

    #[test]
    fn retries_a_failed_connect_did_not_run_wait_for_the_next() {
        let mut retries = Retries::default();
        let mut queued = VecDeque::new();
        retries.ended(vec![card(1), card(2)], &mut queued);
        queued.push_back(card(7));
        retries.requeue(&mut queued);
        // The connection ran 1 and stopped it again, then dropped before 2.
        queued.pop_front();
        assert!(retries.ended(vec![card(1)], &mut queued).is_empty());
        assert_eq!(
            ids(queued.drain(..)),
            [7],
            "only what wasn't a retry is refused"
        );
        retries.requeue(&mut queued);
        assert_eq!(ids(queued.drain(..)), [1, 2]);
    }

    #[test]
    fn a_transfer_that_keeps_stopping_is_given_up_once() {
        let mut retries = Retries::default();
        let mut queued = VecDeque::new();
        for _ in 0..RETRIES {
            assert!(retries.ended(vec![card(1)], &mut queued).is_empty());
            retries.requeue(&mut queued);
            queued.pop_front();
        }
        assert_eq!(ids(retries.ended(vec![card(1)], &mut queued)), [1]);
        retries.requeue(&mut queued);
        assert!(queued.is_empty(), "nothing left to retry");
    }

    #[test]
    fn failed_connects_count_and_finished_transfers_start_over() {
        let mut retries = Retries::default();
        let mut queued = VecDeque::new();
        retries.ended(vec![card(1), card(2)], &mut queued);
        // 1 finished on the next connection; 2 never ran.
        retries.requeue(&mut queued);
        queued.pop_front();
        retries.ended(Vec::new(), &mut queued);
        // 1 stopped again later: a fresh transfer with all its tries; 2 is on its third.
        retries.requeue(&mut queued);
        assert!(retries.ended(vec![card(1)], &mut queued).is_empty());
        for _ in 3..RETRIES {
            retries.requeue(&mut queued);
            assert!(retries.ended(Vec::new(), &mut queued).is_empty());
        }
        retries.requeue(&mut queued);
        assert_eq!(ids(retries.ended(Vec::new(), &mut queued)), [2]);
        retries.requeue(&mut queued);
        assert_eq!(ids(queued.drain(..)), [1]);
    }

    #[test]
    fn a_stopped_share_is_retried_then_refused_after_its_tries() {
        let share = || {
            Command::ShareFiles(
                [7; 32],
                vec![PathBuf::from("a.bin"), PathBuf::from("b.bin")],
            )
        };
        let mut retries = Retries::default();
        let mut queued = VecDeque::new();
        // Each connection takes the requeued share, then stops it again.
        let attempt = |retries: &mut Retries, queued: &mut VecDeque<Command>| {
            retries.requeue(queued);
            queued.clear();
            retries.ended(vec![share()], queued)
        };
        for _ in 0..RETRIES {
            assert!(
                attempt(&mut retries, &mut queued).is_empty(),
                "kept for the next connection"
            );
        }
        let refused = attempt(&mut retries, &mut queued);
        assert!(
            matches!(refused.as_slice(), [Command::ShareFiles(..)]),
            "given up once"
        );
    }
}
