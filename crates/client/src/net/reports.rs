//! Reporting a message (#678): who it goes to, shown before sending, then sending it.
//! The recipients are checked against the members and roles this app shows (see
//! `directory::visible_reviewers`), and the server's owner is pinned on first sight (the
//! first member list loaded), so a change of owner is pointed out before a DM report goes
//! to the new one.

use std::path::{Path, PathBuf};

use anyhow::Result;
use rusttalk_core::{
    auth::{decode, encode},
    client::{
        AlreadyReported, KeyClient, RateLimited, RecipientsChanged, RecipientsUnchecked,
        ReportRefused, Visible,
    },
    reports::Reason,
};
use url::Url;

use super::{Ctx, State, Update, admin, directory};
use crate::server::Id;

/// Who a report would go to.
#[derive(Clone, Debug, PartialEq)]
pub struct Recipients {
    pub people: Vec<Id>,
    /// The owner this device saw before, when someone else is the owner now.
    pub previous_owner: Option<Id>,
    /// The owner shown with them.
    pub owner: Option<Id>,
}

/// A message to report, as the reporter confirmed it.
pub struct Outgoing {
    pub channel: Id,
    pub message: Id,
    pub edit: Option<Id>,
    pub dm: bool,
    pub reason: Reason,
    pub note: String,
    pub recipients: Vec<Id>,
    /// The owner the reporter was shown: nothing is sent if another is the owner now.
    pub owner: Option<Id>,
}

/// `report_owner/<server ID>.json`: the owner this device first saw here.
fn owner_path(data: &Path, server_id: &Id) -> PathBuf {
    data.join("report_owner")
        .join(format!("{}.json", hex::encode(server_id)))
}

fn pinned_owner(data: &Path, server_id: &Id) -> Option<Id> {
    let saved: Option<String> = crate::load_json(&owner_path(data, server_id)).ok()?;
    decode(&saved?).ok()
}

fn pin_owner(data: &Path, server_id: &Id, owner: &Id) {
    let path = owner_path(data, server_id);
    let saved = std::fs::create_dir_all(path.parent().expect("in a folder"))
        .map_err(anyhow::Error::from)
        .and_then(|()| rusttalk_core::client::save_json(data, &path, &Some(encode(owner))));
    if let Err(error) = saved {
        tracing::warn!(error = %format_args!("{error:#}"), "saving the report owner failed");
    }
}

/// Pin the owner of a member list when none is pinned yet, so a later swap of owner is
/// pointed out even before this device's first report.
pub(super) fn pin_first_owner(data: &Path, server_id: &Id, shown: &Visible) {
    if !shown.several_owners
        && let Some(owner) = shown.owner
        && pinned_owner(data, server_id).is_none()
    {
        pin_owner(data, server_id, &owner);
    }
}

/// The owner seen before, if another person is the owner now.
pub fn owner_change(pinned: Option<Id>, current: Option<Id>) -> Option<Id> {
    pinned.filter(|pinned| current.is_some_and(|current| current != *pinned))
}

/// The members and roles this app shows, loading them first when there are none.
async fn visible(ctx: &Ctx, url: &Url, client: &mut KeyClient<'_>, state: &mut State) -> Visible {
    if directory::visible_reviewers(state).is_none()
        && let Err(error) = directory::directory(ctx, url, client, state).await
    {
        tracing::warn!(error = %format_args!("{error:#}"), "loading members for a report failed");
    }
    directory::visible_reviewers(state).unwrap_or_default()
}

/// Who a report would go to. When the server's list doesn't match what this app shows,
/// members and roles are loaded again once, since the app may just be behind.
async fn recipients(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    (channel, dm, sender): (Id, bool, Id),
) -> Result<(Vec<Id>, Visible)> {
    let shown = visible(ctx, url, client, state).await;
    match client
        .report_recipients(&channel, dm, &sender, &shown)
        .await
    {
        Ok(people) => Ok((people, shown)),
        // Only a list that didn't check out may be this app being behind; a refusal or
        // an older server (404) isn't helped by loading members again.
        Err(error) if settled(&error) => Err(error),
        Err(_) => {
            directory::directory(ctx, url, client, state).await?;
            let shown = directory::visible_reviewers(state).unwrap_or_default();
            let people = client
                .report_recipients(&channel, dm, &sender, &shown)
                .await?;
            Ok((people, shown))
        }
    }
}

/// Look up who a report of `sender`'s message would go to, for the report dialog.
pub(super) async fn look_up(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    (channel, message, dm, sender): (Id, Id, bool, Id),
) {
    let found = recipients(ctx, url, client, state, (channel, dm, sender)).await;
    let answer = found
        .map(|(people, shown)| {
            let pinned = pinned_owner(&ctx.data, &state.server_id);
            if pinned.is_none()
                && let Some(owner) = shown.owner
            {
                pin_owner(&ctx.data, &state.server_id, &owner);
            }
            Recipients {
                people,
                previous_owner: owner_change(pinned, shown.owner),
                owner: shown.owner,
            }
        })
        .map_err(|error| refused(&error));
    ctx.send(Update::ReportRecipients(url.clone(), message, answer));
}

/// Send a report to the recipients and owner the reporter confirmed. Once sent, that
/// owner is the one pinned: the reporter was shown any change.
pub(super) async fn send(
    ctx: &Ctx,
    url: &Url,
    client: &mut KeyClient<'_>,
    state: &mut State,
    report: Outgoing,
) {
    let shown = visible(ctx, url, client, state).await;
    let mut sent = attempt(client, &report, shown).await;
    // This app may be behind on members and roles (say, a reviewer added while the
    // dialog was open): load them once and check again, so a change shows as one rather
    // than as a mismatch. Still nothing goes to anyone the reporter wasn't shown.
    if sent.as_ref().is_err_and(|error| !settled(error))
        && directory::directory(ctx, url, client, state).await.is_ok()
    {
        let shown = directory::visible_reviewers(state).unwrap_or_default();
        sent = attempt(client, &report, shown).await;
    }
    if sent.is_ok()
        && let Some(owner) = report.owner
    {
        pin_owner(&ctx.data, &state.server_id, &owner);
    }
    ctx.send(Update::MessageReported(
        sent.map_err(|error| refused(&error)),
    ));
}

/// Send `report` if `shown` still has the owner the reporter was shown.
async fn attempt(client: &mut KeyClient<'_>, report: &Outgoing, shown: Visible) -> Result<()> {
    if shown.owner != report.owner {
        return Err(RecipientsChanged(report.recipients.clone()).into());
    }
    client
        .report(
            &report.channel,
            &report.message,
            report.edit.as_ref(),
            report.dm,
            report.reason,
            &report.note,
            &report.recipients,
            &shown,
        )
        .await
}

/// A refusal that loading members and roles again won't change: anything but a
/// recipient list that didn't check out.
fn settled(error: &anyhow::Error) -> bool {
    !error.is::<RecipientsUnchecked>()
}

/// What went wrong, in plain words.
fn refused(error: &anyhow::Error) -> String {
    if error.is::<AlreadyReported>() {
        t!("reports.already").into()
    } else if error.is::<RateLimited>() {
        t!("reports.slow_down").into()
    } else if error.is::<RecipientsChanged>() {
        t!("reports.recipients_changed").into()
    } else if let Some(ReportRefused(code)) = error.downcast_ref() {
        tracing::info!(code, "report refused");
        match code.as_str() {
            "too_many_reports" => t!("reports.too_many"),
            "report_queue_full" => t!("reports.queue_full"),
            _ => t!("reports.not_sent"),
        }
        .into()
    } else if error.downcast_ref::<reqwest::Error>().is_some() {
        admin::failed(error)
    } else if error.is::<RecipientsUnchecked>() {
        tracing::warn!(error = %format_args!("{error:#}"), "report recipients didn't check out");
        t!("reports.recipients_unchecked").into()
    } else {
        tracing::warn!(error = %format_args!("{error:#}"), "report not sent");
        t!("reports.not_sent").into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_owner_is_pointed_out_until_a_report_pins_them() {
        let (first, second) = ([1; 32], [2; 32]);
        assert_eq!(owner_change(None, Some(first)), None);
        assert_eq!(owner_change(Some(first), Some(first)), None);
        assert_eq!(owner_change(Some(first), Some(second)), Some(first));
        assert_eq!(owner_change(Some(first), None), None);
    }

    #[test]
    fn only_a_list_that_didnt_check_out_is_called_one() {
        // Caps and limits say what they are, and aren't retried (L2).
        let full = anyhow::Error::from(ReportRefused("report_queue_full".into()));
        assert!(settled(&full));
        assert_eq!(refused(&full), t!("reports.queue_full"));
        let mine = anyhow::Error::from(ReportRefused("too_many_reports".into()));
        assert_eq!(refused(&mine), t!("reports.too_many"));
        let shared = anyhow::Error::from(ReportRefused("nonce_reused".into()));
        assert_eq!(refused(&shared), t!("reports.not_sent"));
        let mismatch = anyhow::anyhow!("a listed reviewer can't review reports");
        let mismatch = mismatch.context(RecipientsUnchecked);
        assert!(!settled(&mismatch));
        assert_eq!(refused(&mismatch), t!("reports.recipients_unchecked"));
        // Anything else is no accusation, and isn't retried, when sending or when
        // looking up who it goes to (round 5 L3).
        let other = anyhow::anyhow!("missing message key");
        assert!(settled(&other));
        assert!(settled(&anyhow::Error::from(RateLimited(
            std::time::Duration::ZERO
        ))));
        assert_eq!(refused(&other), t!("reports.not_sent"));
    }

    #[test]
    fn the_owner_is_pinned_on_first_sight_and_kept() {
        let data = tempfile::tempdir().unwrap();
        let (first, second, server) = ([1; 32], [2; 32], [9; 32]);
        let shown = |owner: Id, several_owners: bool| Visible {
            owner: Some(owner),
            several_owners,
            ..Visible::default()
        };
        // Two listed owners pin nobody; the first single owner seen is pinned (H3).
        pin_first_owner(data.path(), &server, &shown(first, true));
        assert_eq!(pinned_owner(data.path(), &server), None);
        pin_first_owner(data.path(), &server, &shown(first, false));
        assert_eq!(pinned_owner(data.path(), &server), Some(first));
        // A swapped owner later doesn't replace it, so the report dialog points it out.
        pin_first_owner(data.path(), &server, &shown(second, false));
        assert_eq!(pinned_owner(data.path(), &server), Some(first));
    }
}
