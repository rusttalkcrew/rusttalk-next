//! A diagram drawn together in the call: hosting ours, and following someone else's.
//! `rusttalk_core::diagram` keeps the order; this sends and receives, and tells the
//! window what to show.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use rusttalk_core::{
    client::voice::ParticipantIdentity,
    diagram::{Feed, Host, Message, Share, Step, Viewer},
};

use super::{Command, Ctx, Update, voice::Live};
use crate::server::Id;

/// Who asks to edit our diagram, and who may, by device, with their person.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Guests {
    pub asking: Vec<(String, Id)>,
    pub editing: Vec<(String, Id)>,
}

/// How often members owed the whole diagram get it.
const TICK: Duration = Duration::from_millis(100);

/// Our diagram session in the call.
pub(super) struct Hosting {
    host: Host,
    pub(super) tick: tokio::time::Interval,
    /// The person behind each device that asked to edit, to name them.
    people: HashMap<String, Id>,
}

/// Whether a command is about diagrams.
pub(super) fn is_diagram(command: &Command) -> bool {
    matches!(
        command,
        Command::StartDiagram(_)
            | Command::StopDiagram
            | Command::JoinDiagram(_)
            | Command::AnswerEdit(..)
            | Command::RevokeEdit(_)
            | Command::AskToEdit
            | Command::StopEditing
            | Command::EditDiagram(_)
    )
}

async fn send(live: &Live, to: &str, message: &Message) {
    let to = ParticipantIdentity::from(to.to_owned());
    if let Err(error) = live.room.send_diagram(&to, message).await {
        tracing::warn!(error = %format_args!("{error:#}"), "sending a diagram message failed");
    }
}

/// Run a diagram command from the window. True when our session started or ended.
pub(super) async fn command(ctx: &Ctx, live: &mut Live, command: Command) -> bool {
    match command {
        Command::StartDiagram(doc) => {
            stop(live).await;
            leave(live).await;
            let host = Host::new(*doc);
            let share = Share { id: host.id };
            if let Err(error) = live.room.set_diagram_share(Some(&share)).await {
                tracing::warn!(error = %format_args!("{error:#}"), "starting a diagram failed");
                ctx.send(Update::DiagramStopped);
                ctx.send(Update::Failed(t!("diagram.failed").into()));
                return false;
            }
            tracing::info!("hosting a diagram");
            ctx.send(Update::Diagram(Feed::Reset(host.doc.clone())));
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            live.diagram = Some(Hosting {
                host,
                tick,
                people: HashMap::new(),
            });
            ctx.send(Update::DiagramGuests(Guests::default()));
            true
        }
        Command::StopDiagram => stop(live).await,
        // One diagram at a time in the window: ours while we host one.
        Command::JoinDiagram(_) if live.diagram.is_some() => false,
        Command::JoinDiagram(host) => {
            leave(live).await;
            let share = host.and_then(|host| {
                let (participant, _) = (live.room.participants().into_iter())
                    .find(|(participant, _)| participant.identity().as_str() == host)?;
                Some((host, live.room.diagram_share(&participant)?))
            });
            if let Some((host, share)) = share {
                tracing::info!("following a diagram");
                let viewer = Viewer::new(&host, share.id);
                send(live, &host, &viewer.join()).await;
                live.diagram_view = Some(viewer);
            }
            false
        }
        Command::AnswerEdit(who, allow) => {
            let Some(hosting) = &mut live.diagram else {
                return false;
            };
            if let Some(reply) = hosting.host.answer(&who, allow, Instant::now()) {
                let granted = matches!(reply, Message::Grant { .. });
                tracing::info!(granted, "answered a request to edit our diagram");
                guests(ctx, live);
                send(live, &who, &reply).await;
            }
            false
        }
        Command::RevokeEdit(who) => {
            let Some(hosting) = &mut live.diagram else {
                return false;
            };
            if let Some(revoke) = hosting.host.revoke(&who) {
                tracing::info!("took back editing our diagram");
                guests(ctx, live);
                send(live, &who, &revoke).await;
            }
            false
        }
        Command::AskToEdit => {
            if let Some(viewer) = &mut live.diagram_view {
                tracing::info!("asking to edit a diagram");
                let (to, ask) = (viewer.host.clone(), viewer.ask());
                send(live, &to, &ask).await;
            }
            false
        }
        Command::StopEditing => {
            if let Some(viewer) = &mut live.diagram_view
                && let Some(withdraw) = viewer.release()
            {
                let to = viewer.host.clone();
                send(live, &to, &withdraw).await;
            }
            false
        }
        Command::EditDiagram(ops) => {
            if let Some(hosting) = &mut live.diagram {
                let applied = hosting.host.edit(ops);
                pass_on(ctx, live, applied).await;
            } else if let Some(viewer) = &mut live.diagram_view {
                let to = viewer.host.clone();
                for message in viewer.edit(ops) {
                    send(live, &to, &message).await;
                }
            }
            false
        }
        _ => false,
    }
}

/// Edits applied to our diagram: to our window, and to everyone following it.
async fn pass_on(ctx: &Ctx, live: &mut Live, applied: Vec<rusttalk_core::diagram::Op>) {
    let Some(hosting) = &mut live.diagram else {
        return;
    };
    if applied.is_empty() {
        return;
    }
    ctx.send(Update::Diagram(Feed::Ops(applied.clone())));
    let messages = hosting.host.broadcast(applied);
    let members: Vec<String> = hosting.host.members().map(str::to_owned).collect();
    for member in members {
        for message in &messages {
            send(live, &member, message).await;
        }
    }
}

/// Tell the window who asks to edit our diagram, and who may.
fn guests(ctx: &Ctx, live: &Live) {
    let Some(hosting) = &live.diagram else {
        return;
    };
    let named = |who: &str| Some((who.to_owned(), *hosting.people.get(who)?));
    let view = Guests {
        asking: (hosting.host.askers(Instant::now()).into_iter())
            .filter_map(named)
            .collect(),
        editing: hosting.host.editors().filter_map(named).collect(),
    };
    ctx.send(Update::DiagramGuests(view));
}

/// End our session: tell the members and the call. True if there was one.
pub(super) async fn stop(live: &mut Live) -> bool {
    let Some(hosting) = live.diagram.take() else {
        return false;
    };
    tracing::info!("ended our diagram");
    let end = Message::End {
        id: hosting.host.id,
    };
    for member in hosting.host.members() {
        send(live, member, &end).await;
    }
    if let Err(error) = live.room.set_diagram_share(None).await {
        tracing::warn!(error = %format_args!("{error:#}"), "announcing the end of a diagram failed");
    }
    true
}

/// Stop following someone else's diagram.
async fn leave(live: &mut Live) {
    if let Some(viewer) = live.diagram_view.take() {
        send(live, &viewer.host, &Message::Leave { id: viewer.id }).await;
    }
}

/// Send the whole diagram to whoever is owed it.
pub(super) async fn flush(live: &mut Live) {
    let Some(hosting) = &mut live.diagram else {
        return;
    };
    let due = hosting.host.due(Instant::now());
    if due.is_empty() {
        return;
    }
    let parts = hosting.host.snapshot();
    for who in due {
        for part in &parts {
            send(live, &who, part).await;
        }
    }
}

/// A sealed diagram message from someone in the call; `person` is who they are, if
/// the server connection is up to tell. Without it nobody may ask to edit, or edit.
pub(super) async fn received(
    ctx: &Ctx,
    live: &mut Live,
    from: &ParticipantIdentity,
    person: Option<Id>,
    wire: &[u8],
) {
    let Some(message) = live.room.open_diagram(from, wire) else {
        // Per message, and anyone in the call can send them: not worth a warning each.
        tracing::debug!("dropped a diagram message that did not authenticate");
        return;
    };
    if let Some(hosting) = &mut live.diagram {
        let mine = message.clone();
        match hosting
            .host
            .receive(from.as_str(), mine, person.is_some(), Instant::now())
        {
            Step::Nothing => {}
            Step::Ask => {
                tracing::info!("someone asks to edit our diagram");
                if let Some(person) = person {
                    hosting.people.insert(from.to_string(), person);
                }
                guests(ctx, live);
            }
            Step::Reply(reply) => {
                tracing::info!("refused a request to edit our diagram");
                send(live, from.as_str(), &reply).await;
            }
            Step::Applied(ops) => pass_on(ctx, live, ops).await,
            Step::Ended => {
                tracing::info!("asking or editing our diagram ended by the viewer");
                guests(ctx, live);
            }
        }
    }
    let Some(viewer) = &mut live.diagram_view else {
        return;
    };
    match viewer.receive(from.as_str(), message) {
        Feed::Nothing => {}
        Feed::Resync => {
            tracing::info!("missed a diagram edit; asking for the diagram again");
            let (to, join) = (viewer.host.clone(), viewer.join());
            send(live, &to, &join).await;
        }
        Feed::Ended => {
            live.diagram_view = None;
            ctx.send(Update::Diagram(Feed::Ended));
        }
        feed => ctx.send(Update::Diagram(feed)),
    }
}

/// Someone left the call: they no longer follow or edit, and their diagram is gone.
pub(super) fn left(ctx: &Ctx, live: &mut Live, who: &str) {
    if let Some(hosting) = &mut live.diagram
        && hosting.host.left(who) == Step::Ended
    {
        tracing::info!("editing our diagram ended: they left the call");
        guests(ctx, live);
    }
    if (live.diagram_view.as_ref()).is_some_and(|viewer| viewer.host == who) {
        live.diagram_view = None;
        ctx.send(Update::Diagram(Feed::Ended));
    }
}
