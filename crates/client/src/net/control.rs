//! Remote control of a shared screen in the call: asking, answering, and the input.
//! `rusttalk_core::control` decides what each message may do; this sends and injects.

use std::time::Instant;

use rusttalk_core::{
    client::voice::ParticipantIdentity,
    control::{Answer, Input, Message, Nonce, Refusal, Step},
    screen::REMOTE_CONTROL,
};

use super::{Command, Ctx, Update, voice::Live};
use crate::server::Id;

/// What the window shows about remote control in the call.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ControlView {
    /// Someone asks to control our screen, and their request, to answer exactly that.
    pub asked: Option<(Id, Nonce)>,
    /// Someone controls our screen.
    pub by: Option<Id>,
    /// The screen (track ID) we asked to control, and whether we control it now.
    pub screen: Option<(String, bool)>,
}

/// Whether a command is about remote control.
pub(super) fn is_control(command: &Command) -> bool {
    matches!(
        command,
        Command::RequestControl(_)
            | Command::ReleaseControl
            | Command::AnswerControl(..)
            | Command::StopControl
            | Command::ControlInput(_)
    )
}

/// Send a control message to one participant. False if it couldn't be sent.
async fn send(live: &Live, to: String, message: &Message) -> bool {
    let to = ParticipantIdentity::from(to);
    let result = live.room.send_control(&to, message).await;
    if let Err(error) = &result {
        tracing::warn!(error = %format_args!("{error:#}"), "sending a remote control message failed");
    }
    result.is_ok()
}

fn inject(live: &Live, inputs: &[Input]) {
    let Some((_, capture, _)) = &live.screen else {
        return;
    };
    let remote = capture.remote();
    for input in inputs {
        // Per event and often harmless (the window is minimized), so not a warning.
        if let Err(error) = remote.inject(input) {
            tracing::debug!(error = %format_args!("{error:#}"), "remote input not injected");
        }
    }
}

/// Bring the window up to date, forgetting what the state machines ended.
fn show(ctx: &Ctx, live: &mut Live) {
    let view = &mut live.control;
    if live.host.asker(Instant::now()).is_none() {
        view.asked = None;
    }
    if live.host.controller().is_none() {
        view.by = None;
    }
    view.screen = (live.guest.screen()).map(|(screen, on)| (screen.to_owned(), on));
    ctx.send(Update::Control(view.clone()));
}

/// A sealed control message arrived from a participant; `person` is who they are, if known.
pub(super) async fn received(
    ctx: &Ctx,
    live: &mut Live,
    from: &ParticipantIdentity,
    person: Option<Id>,
    wire: &[u8],
) {
    let Some(message) = live.room.open_control(from, wire) else {
        tracing::warn!("dropped a remote control message that did not authenticate");
        return;
    };
    let screen = live.guest.screen().map(|(screen, _)| screen.to_owned());
    match live.guest.receive(from.as_str(), &message) {
        Answer::Nothing => {}
        Answer::Granted => {
            tracing::info!("controlling a shared screen");
            show(ctx, live);
            return;
        }
        answer => {
            let why = match answer {
                Answer::Refused(reason) => Some(reason),
                _ => None,
            };
            tracing::info!(?why, "remote control of a shared screen ended");
            show(ctx, live);
            ctx.send(Update::ControlEnded(screen.unwrap_or_default(), why));
            return;
        }
    }
    // We can't name an unknown device in the prompt, so it may not ask.
    let ours = live.screen.as_ref().map(|(sid, ..)| sid.to_string());
    let shared = ours.as_deref().filter(|_| person.is_some());
    let now = Instant::now();
    match live
        .host
        .receive(from.as_str(), message, shared, REMOTE_CONTROL, now)
    {
        Step::Nothing => {}
        Step::Ask => {
            tracing::info!("someone asks to control our screen");
            live.control.asked = person.zip(live.host.request(now));
            show(ctx, live);
        }
        Step::Reply(reply) => {
            if let Message::Deny { reason, .. } = &reply {
                tracing::info!(?reason, "refused a remote control request");
            }
            send(live, from.to_string(), &reply).await;
        }
        Step::Inject(input) => inject(live, &[input]),
        Step::Ended(releases) => {
            tracing::info!("remote control request or session ended by the viewer");
            inject(live, &releases);
            show(ctx, live);
        }
    }
}

/// Run a remote control command from the window.
pub(super) async fn command(ctx: &Ctx, live: &mut Live, command: Command) {
    match command {
        Command::RequestControl(screen) => {
            release(live).await;
            let sharer = live
                .room
                .remote_video_tracks()
                .into_iter()
                .find(|video| video.sid.as_str() == screen)
                .map(|video| video.participant.to_string());
            if let Some(sharer) = sharer {
                tracing::info!("asking to control a shared screen");
                let request = live.guest.ask(&sharer, &screen);
                if !send(live, sharer, &request).await {
                    live.guest = Default::default();
                    ctx.send(Update::Failed("Couldn't ask for control.".into()));
                }
            }
            show(ctx, live);
        }
        Command::ReleaseControl => {
            release(live).await;
            show(ctx, live);
        }
        Command::AnswerControl(request, allow) => {
            let asker = live.control.asked.map(|(person, _)| person);
            if let Some((to, reply)) = live.host.answer(request, allow, Instant::now()) {
                live.control.asked = None;
                if matches!(reply, Message::Grant { .. }) {
                    tracing::info!("allowed remote control of our screen");
                    live.control.by = asker;
                } else {
                    tracing::info!("denied remote control of our screen");
                }
                send(live, to, &reply).await;
            }
            show(ctx, live);
        }
        Command::StopControl => stop_hosting(ctx, live).await,
        Command::ControlInput(inputs) => {
            for input in inputs {
                if let Some((to, message)) = live.guest.input(input) {
                    send(live, to, &message).await;
                }
            }
        }
        _ => {}
    }
}

async fn release(live: &mut Live) {
    if let Some((to, message)) = live.guest.release() {
        tracing::info!("stopped asking for or controlling a shared screen");
        send(live, to, &message).await;
    }
}

/// Take control of our screen back, or turn down an open request: the sharer said so,
/// or the share stopped or changed.
pub(super) async fn stop_hosting(ctx: &Ctx, live: &mut Live) {
    if let Some((to, message, releases)) = live.host.stop() {
        tracing::info!("took back remote control of our screen");
        inject(live, &releases);
        send(live, to, &message).await;
    }
    show(ctx, live);
}

/// Someone left the call: control by them, or of their screen, is over.
pub(super) fn left(ctx: &Ctx, live: &mut Live, who: &str) {
    let hosted = live.host.left(who).map(|releases| inject(live, &releases));
    if hosted.is_some() | live.guest.gone(Some(who), None) {
        tracing::info!("remote control ended: someone left the call");
        show(ctx, live);
    }
}

/// A screen was unpublished: stop asking for, or controlling, it.
pub(super) fn unpublished(ctx: &Ctx, live: &mut Live, screen: &str) {
    if live.guest.gone(None, Some(screen)) {
        tracing::info!("remote control ended: the screen is no longer shared");
        show(ctx, live);
    }
}

/// Leaving the call: let go of whatever a controller still holds on our side.
pub(super) fn leave(live: &mut Live) {
    if let Some((.., releases)) = live.host.stop() {
        inject(live, &releases);
    }
}

/// Why asking for control ended, for a note in the window; `None` when control was taken back.
pub type Ended = Option<Refusal>;
