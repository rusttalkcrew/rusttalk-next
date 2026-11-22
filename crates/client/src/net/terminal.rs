//! A terminal shared in the call: our output goes to whoever watches it, and a terminal
//! we watch is passed to the window to replay. `rusttalk_core::terminal` keeps the order;
//! this sends and receives.

use std::{
    collections::{BTreeSet, HashMap},
    time::{Duration, Instant},
};

use rusttalk_core::{
    client::voice::ParticipantIdentity,
    terminal::{self, Feed, Host, Message, Share, Step, Viewer},
};

use super::{Command, Ctx, Update, voice::Live};
use crate::{
    server::Id,
    terminal::{Out, Shared},
};

/// Who asks to type in our terminal, and who may, by device, with their person.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Guests {
    pub asking: Vec<(String, Id)>,
    pub typing: Vec<(String, Id)>,
}

/// How often output is gathered and sent: often enough to feel live, seldom enough to
/// send it in few messages.
const FLUSH: Duration = Duration::from_millis(33);
/// More output than this per flush (about 1 MB/s) is dropped; watchers get the screen
/// instead, at most twice a second each.
const BURST: usize = 32 * 1024;

/// Our terminal share in the call.
pub(super) struct Hosting {
    host: Host,
    shared: Shared,
    pub(super) tick: tokio::time::Interval,
    /// Watchers who got only a full-screen program's screen; they get everything once
    /// it ends.
    partial: BTreeSet<String>,
    /// The person behind each device that asked to type, to name them.
    people: HashMap<String, Id>,
}

/// Whether a command is about terminal sharing.
pub(super) fn is_terminal(command: &Command) -> bool {
    matches!(
        command,
        Command::ShareTerminal(_)
            | Command::StopTerminal
            | Command::WatchTerminal(_)
            | Command::AnswerTyping(..)
            | Command::RevokeTyping(_)
            | Command::AskToType
            | Command::StopTyping
            | Command::TypeInTerminal(_)
    )
}

async fn send(live: &Live, to: &str, message: &Message) {
    let to = ParticipantIdentity::from(to.to_owned());
    if let Err(error) = live.room.send_terminal(&to, message).await {
        tracing::warn!(error = %format_args!("{error:#}"), "sending a terminal message failed");
    }
}

/// Run a terminal command from the window. True when our share started or stopped.
pub(super) async fn command(ctx: &Ctx, live: &mut Live, command: Command) -> bool {
    match command {
        Command::ShareTerminal(shared) => {
            stop(live).await;
            let host = Host::default();
            let share = Share { id: host.id };
            if let Err(error) = live.room.set_terminal_share(Some(&share)).await {
                tracing::warn!(error = %format_args!("{error:#}"), "sharing a terminal failed");
                ctx.send(Update::TerminalStopped);
                ctx.send(Update::Failed(t!("terminal.failed").into()));
                return false;
            }
            tracing::info!("sharing a terminal");
            let mut tick = tokio::time::interval(FLUSH);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            live.terminal = Some(Hosting {
                host,
                shared,
                tick,
                partial: BTreeSet::new(),
                people: HashMap::new(),
            });
            ctx.send(Update::TerminalGuests(Guests::default()));
            true
        }
        Command::StopTerminal => stop(live).await,
        Command::WatchTerminal(sharer) => {
            if let Some(viewer) = live.viewing.take() {
                send(live, &viewer.sharer, &Message::Unwatch { id: viewer.id }).await;
            }
            let share = sharer.and_then(|sharer| {
                let (participant, _) = (live.room.participants().into_iter())
                    .find(|(participant, _)| participant.identity().as_str() == sharer)?;
                Some((sharer, live.room.terminal_share(&participant)?))
            });
            if let Some((sharer, share)) = share {
                tracing::info!("watching a shared terminal");
                let viewer = Viewer::new(&sharer, share.id);
                send(live, &sharer, &viewer.watch()).await;
                live.viewing = Some(viewer);
            }
            false
        }
        Command::AnswerTyping(who, allow) => {
            let Some(hosting) = &mut live.terminal else {
                return false;
            };
            if let Some(reply) = hosting.host.answer(&who, allow, Instant::now()) {
                let granted = matches!(reply, Message::Grant { .. });
                tracing::info!(granted, "answered a request to type in our terminal");
                guests(ctx, live);
                send(live, &who, &reply).await;
            }
            false
        }
        Command::RevokeTyping(who) => {
            let Some(hosting) = &mut live.terminal else {
                return false;
            };
            if let Some(revoke) = hosting.host.revoke(&who) {
                tracing::info!("took back typing in our terminal");
                guests(ctx, live);
                send(live, &who, &revoke).await;
            }
            false
        }
        Command::AskToType => {
            if let Some(viewer) = &mut live.viewing {
                tracing::info!("asking to type in a shared terminal");
                let (to, ask) = (viewer.sharer.clone(), viewer.ask());
                send(live, &to, &ask).await;
            }
            false
        }
        Command::StopTyping => {
            if let Some(viewer) = &mut live.viewing
                && let Some(withdraw) = viewer.release()
            {
                let to = viewer.sharer.clone();
                send(live, &to, &withdraw).await;
            }
            false
        }
        Command::TypeInTerminal(bytes) => {
            if let Some(viewer) = &mut live.viewing {
                let to = viewer.sharer.clone();
                for message in viewer.input(&bytes) {
                    send(live, &to, &message).await;
                }
            }
            false
        }
        _ => false,
    }
}

/// Tell the window who asks to type in our terminal, and who may.
fn guests(ctx: &Ctx, live: &Live) {
    let Some(hosting) = &live.terminal else {
        return;
    };
    let named = |who: &str| Some((who.to_owned(), *hosting.people.get(who)?));
    let view = Guests {
        asking: (hosting.host.askers(Instant::now()).into_iter())
            .filter_map(named)
            .collect(),
        typing: hosting.host.typists().filter_map(named).collect(),
    };
    ctx.send(Update::TerminalGuests(view));
}

/// Stop our share: tell the watchers and the call. True if there was one.
pub(super) async fn stop(live: &mut Live) -> bool {
    let Some(hosting) = live.terminal.take() else {
        return false;
    };
    tracing::info!("stopped sharing a terminal");
    let end = Message::End {
        id: hosting.host.id,
    };
    for watcher in hosting.host.watchers() {
        send(live, watcher, &end).await;
    }
    if let Err(error) = live.room.set_terminal_share(None).await {
        tracing::warn!(error = %format_args!("{error:#}"), "announcing the end of a terminal share failed");
    }
    true
}

/// Send what our terminal printed since the last time, and screens to whoever needs one.
pub(super) async fn flush(live: &mut Live) {
    let Some(hosting) = &mut live.terminal else {
        return;
    };
    let now = Instant::now();
    let mut sends: Vec<(String, Message)> = Vec::new();
    {
        // The screen must match the output numbered so far, so both happen under one lock.
        let mut inner = hosting.shared.lock();
        let (outs, overflowed) = inner.take();
        if !inner.emulator.alt() {
            for who in std::mem::take(&mut hosting.partial) {
                hosting.host.owe(&who);
            }
        }
        let size: usize = (outs.iter())
            .map(|out| match out {
                Out::Bytes(bytes) => bytes.len(),
                Out::Resize(..) => 0,
            })
            .sum();
        let mut messages = Vec::new();
        if overflowed || size > BURST {
            hosting.host.resync();
        } else {
            for out in outs {
                match out {
                    Out::Bytes(bytes) => messages.extend(terminal::data(&mut hosting.host, &bytes)),
                    Out::Resize(cols, rows) => {
                        messages.push(terminal::resize(&mut hosting.host, cols, rows))
                    }
                }
            }
        }
        let due = hosting.host.due(now);
        for watcher in hosting.host.watchers() {
            if !due.iter().any(|who| who == watcher) {
                sends
                    .extend((messages.iter()).map(|message| (watcher.to_owned(), message.clone())));
            }
        }
        if !due.is_empty() {
            let (screen, alt) = inner.emulator.snapshot();
            let (cols, rows) = inner.emulator.size();
            let parts = terminal::snapshot(&hosting.host, cols, rows, &screen);
            for who in due {
                if alt {
                    hosting.partial.insert(who.clone());
                }
                sends.extend((parts.iter()).map(|part| (who.clone(), part.clone())));
            }
        }
    }
    for (to, message) in sends {
        send(live, &to, &message).await;
    }
}

/// A sealed terminal message from someone in the call; `person` is who they are, if the
/// server connection is up to tell. Without it nobody may ask to type, or type.
pub(super) async fn received(
    ctx: &Ctx,
    live: &mut Live,
    from: &ParticipantIdentity,
    person: Option<Id>,
    wire: &[u8],
) {
    let Some(message) = live.room.open_terminal(from, wire) else {
        // Per message, and anyone in the call can send them: not worth a warning each.
        tracing::debug!("dropped a terminal message that did not authenticate");
        return;
    };
    if let Some(hosting) = &mut live.terminal {
        let mine = message.clone();
        match hosting
            .host
            .receive(from.as_str(), mine, person.is_some(), Instant::now())
        {
            Step::Nothing => {}
            Step::Ask => {
                tracing::info!("someone asks to type in our terminal");
                if let Some(person) = person {
                    hosting.people.insert(from.to_string(), person);
                }
                guests(ctx, live);
            }
            Step::Reply(reply) => {
                tracing::info!("refused a request to type in our terminal");
                send(live, from.as_str(), &reply).await;
            }
            Step::Type(bytes) => hosting.shared.write(&bytes),
            Step::Ended => {
                tracing::info!("asking or typing in our terminal ended by the viewer");
                guests(ctx, live);
            }
        }
    }
    let Some(viewer) = &mut live.viewing else {
        return;
    };
    match viewer.receive(from.as_str(), message) {
        Feed::Nothing => {}
        Feed::Resync => {
            tracing::info!("missed terminal output; asking for the screen again");
            let (to, watch) = (viewer.sharer.clone(), viewer.watch());
            send(live, &to, &watch).await;
        }
        Feed::Ended => {
            live.viewing = None;
            ctx.send(Update::Terminal(Feed::Ended));
        }
        feed => ctx.send(Update::Terminal(feed)),
    }
}

/// Someone left the call: they no longer watch, and their terminal is gone.
pub(super) fn left(ctx: &Ctx, live: &mut Live, who: &str) {
    if let Some(hosting) = &mut live.terminal {
        hosting.partial.remove(who);
        if hosting.host.left(who) == Step::Ended {
            tracing::info!("typing in our terminal ended: they left the call");
            guests(ctx, live);
        }
    }
    if live
        .viewing
        .as_ref()
        .is_some_and(|viewer| viewer.sharer == who)
    {
        live.viewing = None;
        ctx.send(Update::Terminal(Feed::Ended));
    }
}
