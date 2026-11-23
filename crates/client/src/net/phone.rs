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
        pub(in crate::net) stop: CancellationToken,
        /// Hangs up when the room is left.
        _stop: DropGuard,
    }

    fn ended(ctx: &Ctx, generation: u64, why: &str) {
        ctx.send(Update::Phone(
            generation,
            Event::Ended(Some(why.to_owned())),
        ));
    }

    /// Publish the call's track and start calling. True when the roster changed.
    pub(in crate::net) async fn call(ctx: &Ctx, live: &mut Live, dial: Dial) -> bool {
        if rusttalk_core::route::allow_sip().is_err() {
            ended(ctx, dial.generation, t!("route.sip_unavailable"));
            return false;
        }
        if live.phone.is_some() {
            ended(ctx, dial.generation, t!("phone.one_at_a_time"));
            return false;
        }
        if !live.room.can_share_audio() {
            ended(ctx, dial.generation, t!("call.no_talk_here"));
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
                ended(ctx, dial.generation, t!("phone.failed"));
                return false;
            }
        };
        let me = *ctx.identity.certificate.person_id();
        let line = ctx.audio.phone_line((me, Some(dial.label.clone())));
        let stop = CancellationToken::new();
        let updates = ctx.updates.clone();
        let egui = ctx.egui.clone();
        let generation = dial.generation;
        let events = move |event| {
            if updates.send(Update::Phone(generation, event)).is_ok() {
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
            stop: stop.clone(),
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

#[derive(Default)]
pub(super) struct Withdrawals {
    entries: Vec<(rusttalk_core::client::voice::phone::Call, Option<u64>)>,
    since: Option<std::time::Instant>,
    failed: bool,
}

impl Withdrawals {
    pub(super) fn start(&mut self, call: rusttalk_core::client::voice::phone::Call) {
        if !self.entries.iter().any(|(old, _)| old == &call) {
            self.entries.push((call, None));
            self.since.get_or_insert_with(std::time::Instant::now);
        }
    }
    fn status(
        &self,
        call: &rusttalk_core::client::voice::phone::Call,
    ) -> Option<crate::phone::ConsentStatus> {
        self.entries.iter().any(|(old, _)| old == call).then(|| {
            if self.failed
                || self
                    .since
                    .is_some_and(|since| since.elapsed() > std::time::Duration::from_secs(5))
            {
                crate::phone::ConsentStatus::Failed
            } else {
                crate::phone::ConsentStatus::Withdrawing
            }
        })
    }
    fn published(&mut self, revision: u64) {
        for (_, pending) in &mut self.entries {
            *pending = Some(revision);
        }
        self.failed = false;
    }
    fn resolve(
        &mut self,
        safe: impl Fn(&rusttalk_core::client::voice::phone::Call, Option<u64>) -> bool,
    ) {
        self.entries
            .retain(|(call, revision)| !safe(call, *revision));
        if self.entries.is_empty() {
            self.since = None;
            self.failed = false;
        }
    }
}

/// Refresh external-call notices and the exact microphone tracks allowed to leave the room.
pub(super) async fn sync(ctx: &Ctx, url: &url::Url, live: &mut Live) {
    if live.transport_closed {
        return;
    }
    use crate::phone::ConsentStatus;
    let mut calls = live.room.external_phone_calls();
    for call in &live.phone_consents {
        if !calls.contains(call) {
            ctx.audio.pause_phone_withdrawal(true);
            live.phone_withdrawals.start(call.clone());
        }
    }
    live.phone_consents.retain(|call| calls.contains(call));
    let hosted = None;
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let hosted = live.phone.as_ref().map(|phone| &phone.sid).or(hosted);
    let call = hosted.map(|sid| live.room.local_phone_call(sid));
    let (microphones, acknowledgements) = live.room.phone_bridge_state(call.as_ref());
    if hosted.is_some() {
        ctx.audio.phone_consent(microphones);
    } else {
        ctx.audio.end_phone();
    }
    // Acknowledge only after removing excluded audio or tearing down the mixer tap.
    if live.phone_acknowledgements != acknowledgements {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            live.room.acknowledge_phone_withdrawals(&acknowledgements),
        )
        .await
        {
            Ok(Ok(())) => live.phone_acknowledgements = acknowledgements,
            _ => tracing::warn!("acknowledging phone withdrawal failed"),
        }
    }
    let withdrawn: Vec<_> = live
        .phone_withdrawals
        .entries
        .iter()
        .map(|(call, _)| call.clone())
        .collect();
    let announcement = (
        hosted.cloned(),
        live.phone_consents.clone(),
        withdrawn.clone(),
    );
    if live.phone_announcement.as_ref() != Some(&announcement) {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            live.room
                .announce_phone(hosted, &live.phone_consents, &withdrawn),
        )
        .await
        {
            Ok(Ok(revision)) => {
                live.phone_announcement = Some(announcement);
                live.phone_withdrawals.published(revision);
            }
            _ => {
                live.phone_withdrawals.failed = true;
                tracing::warn!("announcing phone consent failed");
            }
        }
    }
    live.phone_withdrawals.resolve(|call, revision| {
        live.room.phone_host_session_ended(call)
            || revision
                .is_some_and(|revision| live.room.phone_withdrawal_acknowledged(call, revision))
    });
    ctx.audio
        .pause_phone_withdrawal(!live.phone_withdrawals.entries.is_empty());
    for (call, _) in &live.phone_withdrawals.entries {
        if !calls.contains(call) {
            calls.push(call.clone());
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
            let status = live.phone_withdrawals.status(&call).unwrap_or_else(|| {
                if live.phone_consents.contains(&call) {
                    ConsentStatus::On
                } else {
                    ConsentStatus::Off
                }
            });
            (call, label, status)
        })
        .collect();
    ctx.send(super::Update::ExternalPhones(
        url.clone(),
        live.channel,
        calls,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phone::ConsentStatus;
    #[test]
    fn failed_or_published_withdrawal_stays_pending_until_safe_ack() {
        let call = rusttalk_core::client::voice::phone::Call {
            host: "h".into(),
            session: "s".into(),
            track: "TR_phone".into(),
        };
        let other = rusttalk_core::client::voice::phone::Call {
            track: "TR_other".into(),
            ..call.clone()
        };
        let mut pending = Withdrawals::default();
        pending.start(call.clone());
        pending.failed = true;
        pending.resolve(|_, _| false);
        assert_eq!(pending.status(&call), Some(ConsentStatus::Failed));
        assert_eq!(pending.status(&other), None);
        pending.published(2);
        pending.resolve(|_, _| false);
        assert_eq!(pending.status(&call), Some(ConsentStatus::Withdrawing));
        pending.resolve(|_, revision| revision.is_some_and(|revision| revision <= 1));
        assert_eq!(pending.entries.len(), 1);
        pending.resolve(|accepted, revision| accepted == &call && revision == Some(2));
        assert!(pending.entries.is_empty());
    }
}
