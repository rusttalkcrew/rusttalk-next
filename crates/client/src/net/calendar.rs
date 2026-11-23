//! The server's events: loaded with the directory, changed from the Events page.

use anyhow::Result;
use reqwest::StatusCode;
use rusttalk_core::{
    calendar::{Answer, AnswerRecord, Details, EventRecord},
    client::{EventRefused, KeyClient},
    messages::MessageError,
};
use url::Url;

use super::{Ctx, Update};
use crate::server::{Calendar, Event, Id};

pub enum Change {
    Add(Details),
    /// The event as it was shown, so what gets approved is what was read.
    Approve(Box<EventRecord>),
    Delete(Id),
    Answer(Id, Answer),
    /// Remind everyone who hasn't answered.
    Remind(Id),
}

/// Fetch and open the events, or `None` on a server from before them.
pub(super) async fn load(client: &KeyClient<'_>) -> Result<Option<Calendar>> {
    let Some(calendar) = client.calendar().await? else {
        return Ok(None);
    };
    let events = calendar
        .events
        .into_iter()
        .map(|record| {
            let mut event = Event {
                details: None,
                waiting_for_key: false,
                answers: Vec::new(),
                waiting_answers: (0..record.answers.len()).collect(),
                record,
            };
            open(&mut event, |record| client.open_event(record));
            open_answers(
                &mut event,
                |_| true,
                |id, record| client.open_answer(id, record),
            );
            event
        })
        .collect();
    Ok(Some(Calendar {
        enabled: calendar.enabled,
        events,
    }))
}

fn open(event: &mut Event, verify: impl FnOnce(&EventRecord) -> Result<Details>) {
    let result = verify(&event.record);
    event.waiting_for_key = matches!(
        result.as_ref().err().and_then(|e| e.downcast_ref()),
        Some(MessageError::MissingKey)
    );
    event.details = result
        .inspect_err(|error| {
            tracing::warn!(error = %format_args!("{error:#}"), "event could not be opened");
        })
        .ok();
}

fn open_answers(
    event: &mut Event,
    has_key: impl Fn(i64) -> bool,
    verify: impl Fn(&Id, &AnswerRecord) -> Result<Answer>,
) -> bool {
    let pending = event.waiting_answers.len();
    event.waiting_answers.retain(|&index| {
        let answer = &event.record.answers[index];
        if !has_key(answer.epoch) {
            return true;
        }
        match verify(&event.record.id, answer) {
            Ok(value) => event.answers.push((answer.person, value)),
            Err(error) if matches!(error.downcast_ref(), Some(MessageError::MissingKey)) => return true,
            Err(error) => tracing::warn!(error = %format_args!("{error:#}"), "event answer could not be opened"),
        }
        false
    });
    event.waiting_answers.len() != pending
}

/// Drop decrypted content signed by a removed device, even while this server is offline.
pub(super) fn revalidate(
    calendar: &mut Calendar,
    revoked: &rusttalk_core::client::revocations::Revocations,
) -> bool {
    let mut changed = false;
    for event in &mut calendar.events {
        if revoked
            .check(&event.record.author, &event.record.author_device)
            .is_err()
        {
            changed |= event.details.take().is_some() || event.waiting_for_key;
            event.waiting_for_key = false;
        }
        for (index, answer) in event.record.answers.iter().enumerate() {
            if revoked.check(&answer.person, &answer.device).is_err() {
                let before = (event.answers.len(), event.waiting_answers.len());
                event.answers.retain(|(person, _)| *person != answer.person);
                event.waiting_answers.retain(|i| *i != index);
                changed |= before != (event.answers.len(), event.waiting_answers.len());
            }
        }
    }
    changed
}

/// Retry only records whose missing epoch has arrived. Verification still runs in full.
pub(super) fn retry(
    calendar: &mut Calendar,
    has_key: impl Fn(i64) -> bool,
    verify: impl Fn(&EventRecord) -> Result<Details>,
    verify_answer: impl Fn(&Id, &AnswerRecord) -> Result<Answer>,
) -> bool {
    let mut changed = false;
    for event in &mut calendar.events {
        if event.waiting_for_key && has_key(event.record.epoch) {
            open(event, &verify);
            changed |= event.details.is_some();
        }
        changed |= open_answers(event, &has_key, &verify_answer);
    }
    changed
}

/// Apply a change and report back, so the form keeps what was typed when it fails.
pub(super) async fn change(ctx: &Ctx, url: &Url, client: &KeyClient<'_>, change: Change) {
    let result = match change {
        Change::Add(details) => client.add_event(&details).await.map(|_| ()),
        Change::Approve(record) => client.approve_event(&record).await,
        Change::Delete(id) => client.delete_event(&id).await,
        Change::Answer(id, answer) => client.answer_event(&id, answer).await,
        Change::Remind(id) => client.remind_event(&id).await,
    };
    // The server's `channel_changed` reloads the directory, and the events with it.
    let result = result.map_err(|error| {
        tracing::warn!(error = %format_args!("{error:#}"), "event change failed");
        problem(&error).to_owned()
    });
    ctx.send(Update::Calendar(url.clone(), result));
}

fn problem(error: &anyhow::Error) -> &'static str {
    let Some(EventRefused { status, code }) = error.downcast_ref::<EventRefused>() else {
        return t!("events.failed");
    };
    match code.as_str() {
        "events_disabled" => t!("events.disabled"),
        "suggestion_limit" => t!("events.suggestion_limit"),
        "event_limit" => t!("events.event_limit"),
        "event_ended" => t!("events.ended_already"),
        "remind_too_soon" => t!("events.remind_too_soon"),
        "event_not_found" | "event_approved" => t!("events.changed"),
        "invalid_event_time" => t!("events.bad_range"),
        "epoch_changed" => t!("admin.key_changing"),
        _ if *status == StatusCode::FORBIDDEN => t!("admin.no_permission"),
        _ => t!("events.failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_event_and_answer_wait_for_their_own_epochs() -> Result<()> {
        use rusttalk_core::{
            auth::{decode_bytes, encode},
            calendar::{EventContext, GRACE},
            identity::DeviceIdentity,
            keys::random_key,
        };

        let (_, identity) = DeviceIdentity::create("calendar test")?;
        let (words, attendee) = DeviceIdentity::create("attendee")?;
        let attendee_cert = &attendee.certificate;
        let key = random_key()?;
        let details = Details {
            title: "Historical event".into(),
            start: 100,
            end: 200,
            ..Default::default()
        };
        let cert = &identity.certificate;
        let context = EventContext {
            server_id: [4; 32],
            id: [1; 32],
            author: *cert.person_id(),
            author_device: cert.signing_key().to_bytes(),
            epoch: 1,
            remove_at: details.end + GRACE,
        };
        let answer_key = random_key()?;
        let answer_context = rusttalk_core::calendar::AnswerContext {
            server_id: context.server_id,
            event_id: context.id,
            epoch: 3,
            device: attendee_cert.signing_key().to_bytes(),
            person: *attendee_cert.person_id(),
        };
        let record = EventRecord {
            reminded_at: None,
            answers: vec![AnswerRecord {
                person: answer_context.person,
                device: answer_context.device,
                certificate: encode(attendee_cert.public_bytes()?),
                epoch: answer_context.epoch,
                envelope: encode(&answer_context.seal(&answer_key, &attendee, Answer::Going)?),
            }],
            id: context.id,
            author: context.author,
            author_device: context.author_device,
            approved: true,
            epoch: context.epoch,
            remove_at: context.remove_at,
            certificate: encode(cert.public_bytes()?),
            envelope: encode(&context.seal(&key, &identity, &details)?),
        };
        let mut event = Event {
            answers: Vec::new(),
            waiting_answers: vec![0],
            record,
            details: None,
            waiting_for_key: false,
        };
        open(&mut event, |_| Err(MessageError::MissingKey.into()));
        open_answers(
            &mut event,
            |_| true,
            |_, _| Err(MessageError::MissingKey.into()),
        );
        let mut calendar = Calendar {
            enabled: true,
            events: vec![event],
        };
        // Names already use epoch two.
        assert!(!retry(
            &mut calendar,
            |epoch| epoch == 2,
            |_| unreachable!(),
            |_, _| unreachable!()
        ));
        assert!(retry(
            &mut calendar,
            |epoch| epoch == 1,
            |record| Ok(context.open(&key, cert, &decode_bytes(&record.envelope)?)?),
            |_, _| unreachable!()
        ));
        assert_eq!(calendar.events[0].details.as_ref(), Some(&details));
        assert!(retry(
            &mut calendar,
            |epoch| epoch == 3,
            |_| unreachable!(),
            |id, answer| {
                assert_eq!(*id, answer_context.event_id);
                Ok(answer_context.open(
                    &answer_key,
                    attendee_cert,
                    &decode_bytes(&answer.envelope)?,
                )?)
            }
        ));
        assert_eq!(
            calendar.events[0].answers,
            [(answer_context.person, Answer::Going)]
        );
        assert!(!retry(
            &mut calendar,
            |_| true,
            |_| unreachable!(),
            |_, _| unreachable!()
        ));
        // Another connection learns a signed revocation after both records were opened.
        // This check runs with no server connection or decryption keys.
        let profile = tempfile::tempdir()?;
        let revoked = rusttalk_core::client::revocations::Revocations::in_profile(profile.path());
        let removal = rusttalk_core::identity::revoke_device(
            &words,
            &answer_context.person,
            answer_context.device,
        )?;
        assert!(revoked.observe(&answer_context.person, &encode(&removal.to_bytes()))?);
        let ctx = eframe::egui::Context::default();
        let net = super::super::Net::new(
            ctx.clone(),
            std::sync::Arc::new(identity),
            profile.path().into(),
            crate::audio::Audio::new(profile.path())?,
            crate::screen::Video::new(ctx),
        )?;
        let mut state = super::super::State {
            last: Some(crate::server::Directory {
                calendar: Some(calendar),
                ..Default::default()
            }),
            ..Default::default()
        };
        let url = Url::parse("https://offline.example")?;
        state.revalidate(&net.ctx, &url, &revoked, None);
        let Update::Directory(address, dir) = net.updates.try_recv()? else {
            anyhow::bail!("expected updated attendance while offline");
        };
        assert_eq!(address, url);
        let event = &dir.calendar.unwrap().events[0];
        assert_eq!(event.details.as_ref(), Some(&details));
        assert!(event.answers.is_empty());
        state.revalidate(&net.ctx, &url, &revoked, None);
        assert!(net.updates.try_recv().is_err());
        Ok(())
    }
}
