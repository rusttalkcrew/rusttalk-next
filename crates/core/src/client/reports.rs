//! Reporting a message, and the reviewers' queue (see `crate::reports`).

use std::collections::HashSet;

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::json;

use super::{KeyClient, RateLimited, chat::kept_files};
use crate::{
    auth::{decode, decode_bytes, encode},
    identity::{DeviceCertificate, PersonId, REVOCATIONS_PER_PERSON, Revocation, random_bytes},
    keys::{Kind, Scope},
    messages::{MessageBody, MessageContext},
    reports::{
        Confirmation, Evidence, MAX_REPORT_TEXT, MAX_REVIEWER_DEVICES, NewReport, OpenedReport,
        Reason, Report, ReportKey, ReportRecord, Resolution, SignedRevocation, WRAP_BYTES, open,
    },
};

/// The server already has an open report of this message from this person.
#[derive(Debug, thiserror::Error)]
#[error("already reported")]
pub struct AlreadyReported;

/// The people a report would go to changed since the reporter saw the list: show the
/// new one before sending.
#[derive(Debug, thiserror::Error)]
#[error("the people who review reports changed")]
pub struct RecipientsChanged(pub Vec<PersonId>);

/// The server's list of who reviews reports doesn't match what the app shows (or names
/// nobody with a device), so nothing was sent.
#[derive(Debug, thiserror::Error)]
#[error("the reviewer list doesn't match what the app shows")]
pub struct RecipientsUnchecked;

/// The server or this app refused the report for a reason that isn't about who it goes
/// to: the server's code (`too_many_reports`, `report_queue_full`, `nonce_reused`,
/// `too_many_reviewers` and so on).
#[derive(Debug, thiserror::Error)]
#[error("report refused: {0}")]
pub struct ReportRefused(pub String);

/// Who this app sees as the owner and as holding `review_reports`, from the member list
/// and roles it shows. A report goes to nobody else, whatever the reviewer list says.
#[derive(Clone, Debug, Default)]
pub struct Visible {
    pub owner: Option<PersonId>,
    pub reviewers: HashSet<PersonId>,
    /// More than one member is listed as the owner: something is wrong, so no report goes
    /// out until it's sorted.
    pub several_owners: bool,
}

/// One open report as a reviewer sees it.
pub struct ReceivedReport {
    pub id: [u8; 32],
    pub channel: [u8; 32],
    pub message: [u8; 32],
    pub reporter: PersonId,
    pub sender: PersonId,
    pub dm: bool,
    pub at: i64,
    /// The verified report, or why it can't be shown.
    pub content: Result<Checked, Unreadable>,
}

/// Why a report can't be shown on this device.
#[derive(Clone, Debug, PartialEq)]
pub enum Unreadable {
    /// It was sealed before this device was added (or for other devices only): another
    /// of this person's devices can open it.
    NotSealedHere,
    /// It failed verification: don't act on it.
    Invalid(String),
}

/// A report whose reporter and messages all verified.
pub struct Checked {
    pub reason: Reason,
    pub note: String,
    /// What was shown: the message's text (or the shown edit's) and its files.
    pub body: MessageBody,
    /// The device that signed the message has since been removed by its owner. The
    /// signature still checks, but a removed device may have been stolen, so the app
    /// warns instead of showing it as verified.
    pub sender_removed: bool,
    /// Whether this reviewer holds the channel key and derived the same message keys; a
    /// mismatch never gets here (see `OpenedReport::confirms`).
    pub confirmation: Confirmation,
}

#[derive(Deserialize)]
struct Reviewers {
    reviewers: Vec<String>,
}

/// The reviewer list must be what the app shows: for a DM the owner alone, otherwise the
/// owner and members it sees holding `review_reports`. A server can't add a listener.
pub fn check_reviewers(dm: bool, listed: &[PersonId], visible: &Visible) -> Result<()> {
    ensure!(!visible.several_owners, "more than one owner is listed");
    let owner = visible.owner.context("no owner shown")?;
    if dm {
        ensure!(listed == [owner], "a DM report goes to the owner alone");
        return Ok(());
    }
    ensure!(
        listed.contains(&owner),
        "the owner isn't listed as a reviewer"
    );
    for person in listed {
        ensure!(
            *person == owner || visible.reviewers.contains(person),
            "a listed reviewer can't review reports"
        );
    }
    Ok(())
}

/// Who a report of `sender`'s message goes to, from the server's reviewer list: checked
/// against what the app shows, and without the sender unless they are the owner. `dm` is
/// what the app shows; it must agree with this device's DM pins, which the server can't
/// change.
pub fn recipients(
    dm: bool,
    pinned_dm: bool,
    listed: &[PersonId],
    sender: &PersonId,
    visible: &Visible,
) -> Result<Vec<PersonId>> {
    ensure!(dm == pinned_dm, "this chat isn't the kind the app shows");
    check_reviewers(dm, listed, visible)?;
    let owner = visible.owner == Some(*sender);
    Ok((listed.iter())
        .filter(|person| dm || owner || *person != sender)
        .copied()
        .collect())
}

/// The device removals served with a report, each checked once against its person's
/// root. More than its sender's and reporter's worth is refused, so a server can't stall
/// the app with them.
pub fn served_removals(records: &[SignedRevocation]) -> Result<Vec<(PersonId, [u8; 32])>> {
    ensure!(
        records.len() <= 2 * REVOCATIONS_PER_PERSON,
        "too many device removals"
    );
    Ok((records.iter())
        .filter(|signed| signed.record.len() <= MAX_REVOCATION_TEXT)
        .filter_map(|signed| {
            let person = decode::<32>(&signed.person).ok()?;
            let removal = Revocation::verify(person, &decode_bytes(&signed.record).ok()?).ok()?;
            Some((person, *removal.device_signing_key()))
        })
        .collect())
}

/// A signed removal, base64url: far less than this (see `client::revocations`).
const MAX_REVOCATION_TEXT: usize = 256;

/// Whether one of `records`, each checked against its person's root, removes `device`.
pub fn removed_by(records: &[SignedRevocation], person: &PersonId, device: &[u8; 32]) -> bool {
    served_removals(records).is_ok_and(|removals| removals.contains(&(*person, *device)))
}

impl KeyClient<'_> {
    /// Who a report of `sender`'s message in `channel` would go to now (see `recipients`).
    pub async fn report_recipients(
        &mut self,
        channel: &[u8; 32],
        dm: bool,
        sender: &PersonId,
        visible: &Visible,
    ) -> Result<Vec<PersonId>> {
        let path = format!("/v1/reports/reviewers?channel={}", encode(channel));
        let reviewers: Reviewers =
            RateLimited::check(self.request(Method::GET, &path).send().await?)?
                .json()
                .await?;
        let listed: Vec<PersonId> = (reviewers.reviewers.iter())
            .map(|person| decode::<32>(person))
            .collect::<Result<_, _>>()?;
        // DMs may not be pinned yet this session: pin those the catalog lists first.
        if !self.dms.contains_key(channel) {
            self.learn_dms().await?;
        }
        let pinned = self.dms.contains_key(channel);
        recipients(dm, pinned, &listed, sender, visible).context(RecipientsUnchecked)
    }

    /// Report a message (and `edit`, the edit of it that is shown) to `confirmed`, the
    /// people the reporter was shown. Both are fetched again and verified before their
    /// keys are disclosed; if the recipients changed meanwhile, nothing is sent.
    #[allow(clippy::too_many_arguments)]
    pub async fn report(
        &mut self,
        channel: &[u8; 32],
        message: &[u8; 32],
        edit: Option<&[u8; 32]>,
        dm: bool,
        reason: Reason,
        note: &str,
        confirmed: &[PersonId],
        visible: &Visible,
    ) -> Result<()> {
        let server = self.server.known_server().server_id;
        let scope = Scope {
            kind: Kind::Channel,
            id: *channel,
        };
        let mut evidence = Vec::new();
        let mut files = Vec::new();
        for id in std::iter::once(message).chain(edit) {
            let record = self.message(channel, id).await?;
            let body = self.open_message(channel, &record).await?;
            // A key shared with another record would open that one too.
            if (self.opened.get(channel)).is_some_and(|opened| opened.shares_prefix(&record)) {
                return Err(ReportRefused("nonce_reused".into()).into());
            }
            // Edits never carry files: only the message's own are disclosed.
            if id == message {
                files.extend(kept_files(&body));
            }
            let sender = self.message_sender(channel, &record).await?;
            let envelope = decode_bytes(&record.envelope)?;
            let context = MessageContext {
                server_id: server,
                channel_id: *channel,
                epoch: record.epoch,
                id: record.id,
                sender_device: record.sender_device,
                person: record.person,
            };
            let key = self
                .key(scope, record.epoch)
                .context("missing message key")?;
            let message_key = context.disclose(key, &sender, &envelope)?;
            evidence.push(Evidence {
                context,
                sender,
                message_key,
                envelope,
            });
        }
        let sender = evidence[0].context.person;
        let report = Report {
            reason,
            note: note.trim().to_owned(),
            evidence,
        };
        // Once more after refreshing the device list, when a reviewer's devices changed.
        for attempt in 0..2 {
            let people = self
                .report_recipients(channel, dm, &sender, visible)
                .await?;
            if people != confirmed {
                return Err(RecipientsChanged(people).into());
            }
            // Every device the server lists for them: sealed to unless this device knows
            // it was removed, which it then proves with the signed removal.
            let mut devices: Vec<&DeviceCertificate> = Vec::new();
            let mut omitted = Vec::new();
            for certificate in (self.devices.values()).filter(|c| people.contains(c.person_id())) {
                if self.revocations.allows(certificate) {
                    devices.push(certificate);
                    continue;
                }
                let person = certificate.person_id();
                let removal = (self.revocations.records(person)?.into_iter())
                    .find(|removal| {
                        removal.device_signing_key() == certificate.signing_key().as_bytes()
                    })
                    .context("a removed reviewer device without its removal")?;
                omitted.push(SignedRevocation {
                    person: encode(person),
                    record: encode(&removal.to_bytes()),
                });
            }
            // A reviewer's new device may not be known here yet.
            if devices.is_empty() && attempt == 0 {
                self.sync_devices().await?;
                continue;
            }
            if devices.is_empty() {
                return Err(
                    anyhow::anyhow!("nobody here can review reports").context(RecipientsUnchecked)
                );
            }
            if devices.len() > MAX_REVIEWER_DEVICES {
                return Err(ReportRefused("too_many_reviewers".into()).into());
            }
            let mut id = [0; 32];
            random_bytes(&mut id)?;
            let (body, wraps) = report.seal(&server, &id, dm, self.identity, &devices)?;
            let new = NewReport {
                id: encode(&id),
                channel_id: encode(channel),
                message_id: encode(message),
                edit_id: edit.map(|edit| encode(edit)),
                file_ids: files.clone(),
                body: encode(&body),
                keys: (wraps.iter())
                    .map(|(device, key)| ReportKey {
                        device: encode(device),
                        key: encode(key),
                    })
                    .collect(),
                omitted,
            };
            let reply = self
                .request(Method::POST, "/v1/reports")
                .json(&new)
                .send()
                .await?;
            if reply.status() == StatusCode::CONFLICT {
                let code: serde_json::Value = reply.json().await.unwrap_or_default();
                match code["error"].as_str() {
                    Some("already_reported") => return Err(AlreadyReported.into()),
                    Some("reviewers_changed") if attempt == 0 => {
                        // Only the device list: a full key sync could wait for seconds.
                        self.sync_devices().await?;
                        continue;
                    }
                    Some("reviewers_changed") => return Err(RecipientsChanged(people).into()),
                    other => {
                        return Err(ReportRefused(other.unwrap_or("conflict").into()).into());
                    }
                }
            }
            RateLimited::check(reply)?;
            return Ok(());
        }
        bail!("the people who review reports keep changing")
    }

    /// The open reports this person may review, each opened and verified here: DM
    /// reports first, then channel reports, each a page at a time up to `MAX_PAGES`, so
    /// a full channel queue never hides a DM report.
    pub async fn reports(&mut self) -> Result<Vec<ReceivedReport>> {
        // A server keeps at most 1,000 open of each kind, all counted: one page spare.
        const MAX_PAGES: usize = 11;
        let mut records: Vec<ReportRecord> = Vec::new();
        for dm in [true, false] {
            let mut last: Option<(i64, String)> = None;
            for _ in 0..MAX_PAGES {
                let path = match &last {
                    Some((at, id)) => format!("/v1/reports?dm={dm}&after_at={at}&after_id={id}"),
                    None => format!("/v1/reports?dm={dm}"),
                };
                let page: Vec<ReportRecord> =
                    RateLimited::check(self.request(Method::GET, &path).send().await?)?
                        .json()
                        .await?;
                let full = page.len() == 100;
                // Only the kind asked for: a server can't list one twice this way.
                ensure!(
                    page.iter().all(|record| record.dm == dm),
                    "the server listed the wrong kind of report"
                );
                last = page.last().map(|record| (record.at, record.id.clone()));
                records.extend(page);
                if !full {
                    break;
                }
            }
        }
        let mut list = Vec::with_capacity(records.len());
        for record in records {
            let content = match record.key.is_some() {
                false => Err(Unreadable::NotSealedHere),
                true => (self.check_report(&record))
                    .map_err(|error| Unreadable::Invalid(format!("{error:#}"))),
            };
            list.push(ReceivedReport {
                id: decode(&record.id)?,
                channel: decode(&record.channel_id)?,
                message: decode(&record.message_id)?,
                reporter: decode(&record.reporter)?,
                sender: decode(&record.sender)?,
                dm: record.dm,
                at: record.at,
                content,
            });
        }
        Ok(list)
    }

    fn check_report(&self, record: &ReportRecord) -> Result<Checked> {
        let server = self.server.known_server().server_id;
        let wrap = record.key.as_deref().context("not sealed to this device")?;
        // Bounded before decoding, whatever the server sends.
        ensure!(
            wrap.len() <= WRAP_BYTES.div_ceil(3) * 4 && record.body.len() <= MAX_REPORT_TEXT,
            "report too large"
        );
        let reporter: PersonId = decode(&record.reporter)?;
        let opened: OpenedReport = open(
            &server,
            &decode(&record.id)?,
            &reporter,
            record.dm,
            self.identity,
            &decode_bytes(wrap)?,
            &decode_bytes(&record.body)?,
        )?;
        // The listing must name what the reporter signed, and the evidence must be the
        // messages the server stored under those IDs.
        opened.check_listing(record)?;
        // Removals this profile knows, and those the server serves with the report, also
        // of people no longer here, each checked against its root.
        let served = served_removals(&record.revocations)?;
        let removed = |person: &PersonId, device: &[u8; 32]| {
            self.revocations.check(person, device).is_err() || served.contains(&(*person, *device))
        };
        ensure!(
            !removed(&reporter, &opened.reporter_device),
            "the reporter's device was removed"
        );
        let sender_removed = (opened.messages.iter())
            .any(|(context, ..)| removed(&context.person, &context.sender_device));
        let confirmation = opened.confirms(|context| {
            let scope = Scope {
                kind: Kind::Channel,
                id: context.channel_id,
            };
            self.key(scope, context.epoch).cloned()
        });
        ensure!(
            confirmation != Confirmation::Mismatch,
            "the disclosed key isn't the one members have"
        );
        let [(_, _, original), ..] = opened.messages.as_slice() else {
            bail!("empty report");
        };
        let (_, _, shown) = opened.messages.last().expect("one at least");
        // Edits never change files: they stay the original's.
        let mut body = shown.clone();
        body.attachments.clone_from(&original.attachments);
        Ok(Checked {
            reason: opened.reason,
            note: opened.note,
            body,
            sender_removed,
            confirmation,
        })
    }

    /// Close a report: dismiss it, delete the message (and `files`, its stored files as
    /// the verified report shows them), or note the timeout, kick or ban done through
    /// its own request.
    pub async fn resolve_report(
        &self,
        id: &[u8; 32],
        action: Resolution,
        files: &[String],
    ) -> Result<()> {
        let path = format!("/v1/reports/{}/resolve", encode(id));
        RateLimited::check(
            self.request(Method::POST, &path)
                .json(&json!({ "action": action, "files": files }))
                .send()
                .await?,
        )?;
        Ok(())
    }

    /// Dismiss every open channel report by `reporter`, such as a spammer's, or with
    /// `dm` (the owner) every DM report. Returns how many the server closed.
    pub async fn dismiss_reports_from(&self, reporter: &PersonId, dm: bool) -> Result<usize> {
        #[derive(Deserialize)]
        struct Dismissed {
            reports: usize,
        }
        let path = format!("/v1/reports/from/{}/dismiss?dm={dm}", encode(reporter));
        let reply = RateLimited::check(self.request(Method::POST, &path).send().await?)?;
        Ok(reply.json::<Dismissed>().await?.reports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{DeviceIdentity, revoke_device};

    fn visible(owner: PersonId, reviewers: &[PersonId]) -> Visible {
        Visible {
            owner: Some(owner),
            reviewers: reviewers.iter().copied().collect(),
            several_owners: false,
        }
    }

    #[test]
    fn reports_go_only_to_reviewers_the_app_shows() {
        let (owner, moderator, stranger) = ([1; 32], [2; 32], [3; 32]);
        let shown = visible(owner, &[moderator]);
        assert!(check_reviewers(false, &[owner, moderator], &shown).is_ok());
        assert!(check_reviewers(false, &[owner], &shown).is_ok());
        // An extra listener, or the owner left out.
        assert!(check_reviewers(false, &[owner, moderator, stranger], &shown).is_err());
        assert!(check_reviewers(false, &[moderator], &shown).is_err());
        // A DM report: the owner alone.
        assert!(check_reviewers(true, &[owner], &shown).is_ok());
        assert!(check_reviewers(true, &[owner, moderator], &shown).is_err());
        assert!(check_reviewers(true, &[moderator], &shown).is_err());
        assert!(check_reviewers(true, &[owner], &Visible::default()).is_err());
        // A second owner (say, a hidden member given the role) stops everything.
        let doubled = Visible {
            several_owners: true,
            ..shown.clone()
        };
        assert!(check_reviewers(true, &[owner], &doubled).is_err());
        assert!(check_reviewers(false, &[owner, moderator], &doubled).is_err());
    }

    #[test]
    fn recipients_follow_the_pinned_chat_kind_and_leave_out_the_sender() {
        let (owner, moderator, sender) = ([1; 32], [2; 32], [3; 32]);
        let shown = visible(owner, &[moderator, sender]);
        let listed = [owner, moderator, sender];
        // The reported moderator gets nothing; the owner always does.
        assert_eq!(
            recipients(false, false, &listed, &sender, &shown).unwrap(),
            [owner, moderator]
        );
        assert_eq!(
            recipients(false, false, &[owner, moderator], &owner, &shown).unwrap(),
            [owner, moderator]
        );
        // The chat kind must agree with this device's DM pins, before or after the server
        // says otherwise.
        assert!(recipients(true, false, &[owner], &sender, &shown).is_err());
        assert!(recipients(false, true, &listed, &sender, &shown).is_err());
        assert_eq!(
            recipients(true, true, &[owner], &sender, &shown).unwrap(),
            [owner]
        );
    }

    #[test]
    fn served_removals_count_only_when_signed_by_the_person() {
        let (words, person) = DeviceIdentity::create("sender").unwrap();
        let person_id = *person.certificate.person_id();
        let device = person.certificate.signing_key().to_bytes();
        let removal = revoke_device(&words, &person_id, device).unwrap();
        let signed = |person: &PersonId| SignedRevocation {
            person: encode(person),
            record: encode(&removal.to_bytes()),
        };
        assert!(removed_by(&[signed(&person_id)], &person_id, &device));
        assert!(!removed_by(&[signed(&person_id)], &person_id, &[9; 32]));
        // Claimed for someone else, it proves nothing.
        let (_, other) = DeviceIdentity::create("other").unwrap();
        let other_id = *other.certificate.person_id();
        assert!(!removed_by(&[signed(&other_id)], &other_id, &device));
        // More than two people's worth is refused unchecked (L5).
        let flood = vec![signed(&person_id); 2 * REVOCATIONS_PER_PERSON + 1];
        assert!(served_removals(&flood).is_err());
        assert!(!removed_by(&flood, &person_id, &device));
        assert!(served_removals(&flood[1..]).is_ok());
    }
}
