//! The phone call bridged into our voice room (see `crate::phone`). It is published
//! like a shared app, with its sealed name, so members can listen to it and see it
//! under us; older apps show it as shared app audio. Desktop only.

use super::{Command, Ctx, voice::Live};

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(super) use desktop::*;

pub(super) fn is_phone(command: &Command) -> bool {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    return matches!(command, Command::PhoneCall(_));
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = command;
        false
    }
}

/// Phones never call out; their `Live` has no call to stop.
#[cfg(any(target_os = "android", target_os = "ios"))]
pub(super) async fn stop_ended(_ctx: &Ctx, _live: &mut Live) -> bool {
    false
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod desktop {
    use livekit::prelude::TrackSid;
    use tokio::task::JoinHandle;
    use tokio_util::sync::{CancellationToken, DropGuard};

    use super::{Ctx, Live};
    use crate::{
        net::Update,
        phone::call::{Dial, Event, run},
    };

    pub(in crate::net) struct Phone {
        pub(in crate::net) sid: TrackSid,
        pub(in crate::net) label: String,
        task: JoinHandle<()>,
        /// Hangs up when the room is left.
        _stop: DropGuard,
    }

    fn ended(ctx: &Ctx, why: &str) {
        ctx.send(Update::Phone(Event::Ended(Some(why.to_owned()))));
    }

    /// Publish the call's track and start calling. True when the roster changed.
    pub(in crate::net) async fn call(ctx: &Ctx, live: &mut Live, dial: Dial) -> bool {
        if live.phone.is_some() {
            ended(ctx, t!("phone.one_at_a_time"));
            return false;
        }
        if !live.room.can_share_audio() {
            ended(ctx, t!("call.no_talk_here"));
            return false;
        }
        let source = crate::phone::call::source();
        let publication = match live
            .room
            .publish_app_audio(&dial.label, source.clone())
            .await
        {
            Ok(publication) => publication,
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "publishing the phone call failed");
                ended(ctx, t!("phone.failed"));
                return false;
            }
        };
        let me = *ctx.identity.certificate.person_id();
        let line = ctx.audio.phone_line((me, Some(dial.label.clone())));
        let stop = CancellationToken::new();
        let updates = ctx.updates.clone();
        let egui = ctx.egui.clone();
        let events = move |event| {
            if updates.send(Update::Phone(event)).is_ok() {
                egui.request_repaint();
            }
        };
        let label = dial.label.clone();
        tracing::info!("phone call started");
        let task = tokio::spawn(run(dial, line, source, stop.clone(), events));
        live.phone = Some(Phone {
            sid: publication.sid(),
            label,
            task,
            _stop: stop.drop_guard(),
        });
        true
    }

    /// Take the call's track down once the call is over. True when it did.
    pub(in crate::net) async fn stop_ended(_ctx: &Ctx, live: &mut Live) -> bool {
        let Some(phone) = live.phone.take_if(|phone| phone.task.is_finished()) else {
            return false;
        };
        if let Err(error) = live.room.unpublish_audio(&phone.sid).await {
            tracing::warn!(error = %format_args!("{error:#}"), "stopping the phone call's sound failed");
        }
        true
    }
}

/// Refresh external-call notices and the exact microphone tracks allowed to leave the room.
pub(super) async fn sync(ctx: &Ctx, url: &url::Url, live: &mut Live) {
    let calls = live.room.external_phone_calls();
    live.phone_consents.retain(|call| calls.contains(call));
    let hosted = None;
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let hosted = live.phone.as_ref().map(|phone| &phone.sid).or(hosted);
    if let Some(sid) = hosted {
        ctx.audio.phone_consent(
            live.room
                .phone_microphones(&live.room.local_phone_call(sid)),
        );
    } else {
        ctx.audio.end_phone();
    }
    let announcement = (hosted.cloned(), live.phone_consents.clone());
    if live.phone_announcement.as_ref() != Some(&announcement) {
        match live.room.announce_phone(hosted, &live.phone_consents).await {
            Ok(()) => live.phone_announcement = Some(announcement),
            Err(_) => tracing::warn!("announcing phone consent failed"),
        }
    }
    let names = live.room.shared_audio();
    let calls = calls
        .into_iter()
        .map(|call| {
            let label = names
                .iter()
                .find(|(_, sid, _)| sid.as_str() == call.track)
                .map(|(_, _, name)| name.clone())
                .unwrap_or_default();
            let allowed = live.phone_consents.contains(&call);
            (call, label, allowed)
        })
        .collect();
    ctx.send(super::Update::ExternalPhones(
        url.clone(),
        live.channel,
        calls,
    ));
}
