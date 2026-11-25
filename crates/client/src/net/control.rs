//! Remote control of a shared screen in the call: asking, answering, and the input.
//! `rusttalk_core::control` decides what each message may do; this sends and injects.

use std::time::Instant;

use rusttalk_core::{
    client::voice::{ConnectionState, ControlOffer, ParticipantIdentity},
    control::{Answer, Input, Message, Nonce, Refusal, Step},
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
    for input in inputs {
        // Per event and often harmless (the window is minimized), so not a warning.
        if let Err(error) = capture.inject(input) {
            tracing::debug!(error = %format_args!("{error:#}"), "remote input not injected");
        }
    }
}

fn end_input(live: &Live, releases: &[Input]) {
    if !cfg!(target_os = "linux") {
        inject(live, releases);
    }
    // Linux drops its virtual devices; macOS also lets go of whatever it still holds.
    if let Some((_, capture, _)) = &live.screen {
        capture.set_control_active(false);
    }
}

/// What viewers should know about control requests for our screen: `supported` is
/// whether the capture can take input, `chosen` whether the sharer allowed requests.
fn offer(supported: bool, chosen: bool) -> ControlOffer {
    let offered = rusttalk_core::screen::linux_remote_control()
        || rusttalk_core::screen::mac_remote_control();
    if supported {
        ControlOffer::On
    } else if offered && !chosen {
        ControlOffer::Off
    } else {
        ControlOffer::Unsupported
    }
}

/// Tell viewers whether our screen takes control requests, when that changed, so
/// "Request control" is off for them rather than refused.
pub(super) async fn announce_offer(live: &mut Live) {
    if live.transport_closed || live.room.connection_state() != ConnectionState::Connected {
        return;
    }
    let Some((sid, capture, options)) = &live.screen else {
        return;
    };
    let offer = offer(capture.supports_control(), options.remote_control);
    let state = (sid.clone(), offer, live.room.epoch());
    if live.announced_offer.as_ref() == Some(&state) {
        return;
    }
    let announce = live.room.announce_control_offer(sid, offer);
    match tokio::time::timeout(std::time::Duration::from_secs(2), announce).await {
        Ok(Ok(())) => live.announced_offer = Some(state),
        Ok(Err(error)) => {
            tracing::warn!(error = %format_args!("{error:#}"), "announcing control requests failed")
        }
        Err(_) => tracing::warn!("announcing control requests timed out"),
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
    let supported = live
        .screen
        .as_ref()
        .is_some_and(|(_, capture, _)| capture.supports_control())
        && matches!(live.swap, crate::screen::Swap::Idle);
    let now = Instant::now();
    match live
        .host
        .receive(from.as_str(), message, shared, supported, now)
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
            end_input(live, &releases);
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
                    ctx.send(Update::Failed(t!("remote.ask_failed").into()));
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
            let capture = live.screen.as_ref().map(|(_, capture, _)| capture);
            let allow = allow && capture.is_some_and(|capture| capture.supports_control());
            if let Some((to, reply)) = live.host.answer(request, allow, Instant::now()) {
                live.control.asked = None;
                if matches!(reply, Message::Grant { .. }) {
                    tracing::info!("allowed remote control of our screen");
                    live.control.by = asker;
                    if let Some(capture) = capture {
                        capture.set_control_active(true);
                    }
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
        end_input(live, &releases);
        send(live, to, &message).await;
    }
    show(ctx, live);
}

/// Someone left the call: control by them, or of their screen, is over.
pub(super) fn left(ctx: &Ctx, live: &mut Live, who: &str) {
    let hosted = live
        .host
        .left(who)
        .map(|releases| end_input(live, &releases));
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
        end_input(live, &releases);
    }
}

/// Why asking for control ended, for a note in the window; `None` when control was taken back.
pub type Ended = Option<Refusal>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offer_says_off_only_when_the_sharer_could_have_allowed_it() {
        assert_eq!(offer(true, true), ControlOffer::On);
        // An allowed share that can't take input is unsupported, not off.
        assert_eq!(offer(false, true), ControlOffer::Unsupported);
        let opt_in = rusttalk_core::screen::linux_remote_control()
            || rusttalk_core::screen::mac_remote_control();
        let off = if opt_in {
            ControlOffer::Off
        } else {
            ControlOffer::Unsupported
        };
        assert_eq!(offer(false, false), off);
    }
}
